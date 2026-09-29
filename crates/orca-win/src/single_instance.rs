//! Single instance: a named mutex decides who is primary, a named pipe carries
//! "show me" from later launches to the primary.
//!
//! # Why both
//!
//! The mutex alone answers "am I first?" and nothing else. A launcher also
//! needs the second behaviour: a second launch — from a shortcut, from a file
//! association, from the tray menu — should *raise the existing window* and
//! exit, not start a second copy that fights over the same hotkey. That needs
//! a channel, and a named pipe is the one channel that needs no port, no
//! window class, and no shared memory.
//!
//! # The startup race
//!
//! There is an unavoidable gap: the primary wins the mutex, then has to create
//! the pipe. A second launch landing in that window finds the mutex taken and
//! the pipe not yet there. Treating "pipe missing" as "nothing is running" would
//! start a duplicate, which is the exact failure this module exists to prevent.
//! So [`Win32SingleInstance::notify_primary`] retries until a deadline, and
//! reports [`SingleInstanceError::PrimaryNotListening`] when the primary never
//! answers — a *different* answer from "no primary exists", and the caller must
//! not conflate them.
//!
//! # Why the pipe is overlapped
//!
//! Because the listener has to be stoppable. A blocking `ConnectNamedPipe`
//! cannot be interrupted from another thread, so a launcher that used one could
//! never finish shutting down: the thread would sit in the connect forever. The
//! alternative — `CancelSynchronousIo` against a raw thread handle — means
//! hand-rolling thread creation to obtain a handle `std::thread` does not give
//! out. Overlapped I/O plus a stop event is the documented answer, and the
//! client pays a matching cost for it: a pipe instance is in overlapped mode
//! for everyone, so the client's `WriteFile` is overlapped too.

use std::fmt;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, NAMED_PIPE_MODE,
    PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, ReleaseMutex, SetEvent, WaitForMultipleObjects,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::wide::wide_nul;

/// Exit code the process should return when it determined it was a second
/// launch and handed off to the primary.
///
/// A launcher is normally started by a shortcut or a shell action whose caller
/// may inspect the exit code, and "I did the right thing and exited" is worth
/// distinguishing from "I failed to start".
pub const SECONDARY_INSTANCE_EXIT_CODE: i32 = 2;

/// Default object names. `Local\` keeps the mutex out of other terminal
/// sessions, which is what you want: a second RDP session should be a second
/// launcher, not a second client of the first one's.
pub const DEFAULT_MUTEX_NAME: &str = r"Local\orca-launcher-single-instance";
/// `\\.\pipe\` is the Win32 named-pipe namespace, not a filesystem path.
pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\orca-launcher-single-instance";

/// How long a second launch waits for a primary that is still starting up.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

/// Interval between attempts to reach a pipe that is not up yet.
const RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// Longest a connected client may stay silent before the listener gives up on
/// it. Without a bound, one stuck client would wedge the primary for good.
const CLIENT_READ_TIMEOUT_MS: u32 = 2_000;

const INFINITE: u32 = 0xFFFF_FFFF;

// ---------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------

/// A message a later launch sends to the running primary.
///
/// One variant today because one is all the launcher needs. The wire format is
/// versioned and fixed-length rather than a single byte, so adding a variant
/// cannot silently re-interpret an older primary's byte stream, and a truncated
/// write is detectable instead of decoding as a valid command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceCommand {
    /// Show and focus the launcher window.
    Show,
}

impl InstanceCommand {
    const SHOW: u8 = 0x01;
    /// Fixed 4-byte header: opcode, format version, and two reserved bytes.
    const HEADER_LEN: usize = 4;
    const VERSION: u8 = 1;

    fn opcode(self) -> u8 {
        match self {
            InstanceCommand::Show => InstanceCommand::SHOW,
        }
    }

    /// Encodes the command as the complete byte stream to write to the pipe.
    pub fn encode(self) -> Vec<u8> {
        let mut bytes = vec![0u8; InstanceCommand::HEADER_LEN];
        bytes[0] = self.opcode();
        bytes[1] = InstanceCommand::VERSION;
        bytes
    }

    /// Decodes a frame read back from the pipe.
    ///
    /// `None` for anything malformed, including a short read. The caller skips
    /// the frame rather than guessing at it.
    pub fn decode(bytes: &[u8]) -> Option<InstanceCommand> {
        if bytes.len() < InstanceCommand::HEADER_LEN {
            return None;
        }
        if bytes[1] != InstanceCommand::VERSION {
            return None;
        }
        match bytes[0] {
            InstanceCommand::SHOW => Some(InstanceCommand::Show),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Something went wrong establishing or using the primary instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleInstanceError {
    /// The primary holds the mutex but its pipe never became available within
    /// the handshake deadline.
    ///
    /// Distinct from "no primary exists": the caller must still exit, because
    /// the mutex is genuinely held, but it should say the message did not
    /// arrive rather than claim it did.
    PrimaryNotListening,
    /// Not the primary instance. The launcher should show its window and exit.
    AlreadyRunning,
    /// Windows reported a failure for a reason we do not model.
    SystemFailure {
        /// The raw Win32 error code, as `GetLastError()` reported it.
        code: u32,
    },
}

impl fmt::Display for SingleInstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SingleInstanceError::PrimaryNotListening => write!(
                f,
                "another orca instance holds the lock but never answered on its pipe"
            ),
            SingleInstanceError::AlreadyRunning => write!(f, "another orca instance is running"),
            SingleInstanceError::SystemFailure { code } => {
                write!(f, "single-instance call failed (Win32 error {code})")
            }
        }
    }
}

impl std::error::Error for SingleInstanceError {}

/// A Win32 `HANDLE` that is allowed to cross a thread boundary.
///
/// `HANDLE` is a raw pointer, so it is not `Send`, and that is the right default
/// for a pointer. But a Win32 handle is not a pointer into shared memory: it is
/// a kernel object reference that any thread in the process may use, and every
/// API this crate calls on one — `SetEvent`, `ConnectNamedPipe`, `CancelIoEx` —
/// is documented as callable from a thread other than the one that created it.
/// Mutexes and events are the textbook case: signalling a wait from a different
/// thread is the entire point of them.
///
/// What is *not* safe is two threads issuing operations on the same handle at
/// once, or closing it twice. Both are prevented structurally rather than by the
/// marker: every method that touches a handle takes `&mut self`, the guard is
/// single-owner, the listener never closes the stop event, and exactly one of
/// the listener's start-failure paths or the guard's `stop` closes it.
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);

// SAFETY: see the type's documentation. The invariant is enforced by the
// `&mut self` signatures on every method that touches a handle.
unsafe impl Send for SendHandle {}

impl SendHandle {
    /// Takes ownership of a live handle.
    fn new(handle: HANDLE) -> SendHandle {
        SendHandle(handle)
    }

    fn get(&self) -> HANDLE {
        self.0
    }
}

/// The raw Win32 error code behind a `windows::core::Error`.
///
/// `HRESULT_FROM_WIN32(x)` is `0x8007_0000 | x` for codes that fit in 16 bits,
/// which is every code the calls in this module surface. Anything else is
/// returned unchanged, so a caller still sees a real value instead of a
/// plausible-looking one.
pub(crate) fn last_error_code(error: &windows::core::Error) -> u32 {
    const HRESULT_FROM_WIN32: u32 = 0x8007_0000;

    let hresult = error.code().0 as u32;
    if hresult & 0xFFFF_0000 == HRESULT_FROM_WIN32 {
        hresult & 0x0000_FFFF
    } else {
        hresult
    }
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

/// Decides whether this process is the primary launcher, and carries "show"
/// from later launches to it.
///
/// `Drop` releases the mutex and stops the listener. That ordering matters for
/// the crash case: a named mutex is released by the kernel when its last handle
/// closes, so a crashed primary does not wedge the next launch.
pub struct Win32SingleInstance {
    mutex_name: String,
    pipe_name: String,
    /// `Some` only while this process holds the mutex. Holding it is what
    /// "primary" means; nothing else records the role.
    mutex: Option<SendHandle>,
    listener: Option<Listener>,
}

impl Default for Win32SingleInstance {
    fn default() -> Self {
        Self::new()
    }
}

impl Win32SingleInstance {
    /// Creates a guard with the default object names and handshake timeout.
    ///
    /// Claims nothing: no Win32 call is made until
    /// [`Win32SingleInstance::acquire`].
    pub fn new() -> Win32SingleInstance {
        Self::with_names(DEFAULT_MUTEX_NAME, DEFAULT_PIPE_NAME)
    }

    /// Creates a guard with explicit object names.
    ///
    /// The names are parameters because they are the only way two tests can
    /// coexist: a named object is process-global, so a fixed name would let one
    /// test's mutex decide another test's answer.
    pub fn with_names(mutex_name: &str, pipe_name: &str) -> Win32SingleInstance {
        Win32SingleInstance {
            mutex_name: mutex_name.to_owned(),
            pipe_name: pipe_name.to_owned(),
            mutex: None,
            listener: None,
        }
    }

    /// The named mutex this guard competes for.
    pub fn mutex_name(&self) -> &str {
        &self.mutex_name
    }

    /// The named pipe the primary listens on.
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    /// Attempts to become the primary instance.
    ///
    /// `Ok(true)` means this process won the mutex and should continue starting
    /// up. `Ok(false)` means another instance is primary: send
    /// [`Win32SingleInstance::notify_primary`] and exit with
    /// [`SECONDARY_INSTANCE_EXIT_CODE`].
    pub fn acquire(&mut self) -> Result<bool, SingleInstanceError> {
        let name = wide_nul(&self.mutex_name);
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the
        // call, and a null SECURITY_ATTRIBUTES means the default DACL. The
        // handle is kept for as long as this process is primary, and the kernel
        // releases it when the handle closes — which is what makes a crashed
        // primary recoverable.
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }.map_err(|e| {
            SingleInstanceError::SystemFailure {
                code: last_error_code(&e),
            }
        })?;

        // CreateMutexW succeeds even when it merely *opened* an existing mutex.
        // The last-error snapshot is the only way to tell the two apart, so it
        // has to be read with nothing in between to clobber it.
        let existed = unsafe { windows::Win32::Foundation::GetLastError() };

        if existed == ERROR_ALREADY_EXISTS {
            // SAFETY: `handle` is a live handle just returned by CreateMutexW,
            // and this is its only close.
            unsafe { CloseHandle(handle) }.map_err(|e| SingleInstanceError::SystemFailure {
                code: last_error_code(&e),
            })?;
            return Ok(false);
        }

        self.mutex = Some(SendHandle::new(handle));
        Ok(true)
    }

    /// Whether this process currently holds the primary role.
    pub fn is_primary(&self) -> bool {
        self.mutex.is_some()
    }

    /// Gives up the primary role, stopping the listener first.
    ///
    /// Idempotent: releasing when nothing is held is a no-op, not an error.
    /// Unlike the hotkey, "not primary" is a normal state between launches, not
    /// a mistake worth reporting.
    pub fn release(&mut self) -> Result<(), SingleInstanceError> {
        self.stop_listener();
        match self.mutex.take() {
            None => Ok(()),
            // SAFETY: `handle` came from CreateMutexW and left `self.mutex`
            // here, so it is live and this is its only close.
            Some(handle) => unsafe {
                let _ = ReleaseMutex(handle.get());
                CloseHandle(handle.get())
            }
            .map_err(|e| SingleInstanceError::SystemFailure {
                code: last_error_code(&e),
            }),
        }
    }

    /// Starts listening for commands from later launches. Primary only.
    ///
    /// Calling it twice stops the first listener before starting a second, so
    /// two accepts can never race on one pipe.
    pub fn start_listener(
        &mut self,
        on_command: Box<dyn FnMut(InstanceCommand) + Send + 'static>,
    ) -> Result<(), SingleInstanceError> {
        if !self.is_primary() {
            return Err(SingleInstanceError::AlreadyRunning);
        }
        self.stop_listener();

        // The stop event is created and owned here, not on the listener thread,
        // so `stop_listener` can signal it from any thread.
        // SAFETY: a null SECURITY_ATTRIBUTES means the default DACL. Manual
        // reset with an initial false state is correct: this is a level
        // trigger for "please stop", not an edge.
        let stop_event = unsafe { CreateEventW(None, true, false, None) }.map_err(|e| {
            SingleInstanceError::SystemFailure {
                code: last_error_code(&e),
            }
        })?;

        let pipe_name = self.pipe_name.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), SingleInstanceError>>();
        // Copied, not moved: this guard still owns the close, whether the thread
        // started or not.
        let stop_for_thread = SendHandle::new(stop_event);

        let join = std::thread::Builder::new()
            .name("orca-ipc-listener".into())
            .spawn(move || listener_body(pipe_name, on_command, stop_for_thread, ready_tx))
            .map_err(|_| SingleInstanceError::SystemFailure { code: 0 })?;

        // Never hand back a live thread that is not yet listening: the caller
        // would then believe a second launch could reach it.
        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.listener = Some(Listener {
                    stop_event: SendHandle::new(stop_event),
                    join: Some(join),
                });
                Ok(())
            }
            Ok(Err(e)) => {
                // The thread has already returned; close the event it was given.
                // SAFETY: live and owned here, since the thread is joined first.
                unsafe {
                    let _ = CloseHandle(stop_event);
                }
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                // SAFETY: as above.
                unsafe {
                    let _ = CloseHandle(stop_event);
                }
                let _ = join.join();
                Err(SingleInstanceError::SystemFailure { code: 0 })
            }
        }
    }

    /// Stops the listener if one is running. Idempotent.
    pub fn stop_listener(&mut self) {
        if let Some(listener) = self.listener.take() {
            listener.stop();
        }
    }

    /// Sends a command to the primary, waiting for it to come up.
    ///
    /// Secondary instances call this and then exit. The retry loop is the
    /// point: see the module docs on the startup race.
    pub fn notify_primary(
        &self,
        command: InstanceCommand,
        timeout: Duration,
    ) -> Result<(), SingleInstanceError> {
        let deadline = Instant::now() + timeout;
        let payload = command.encode();
        let name = wide_nul(&self.pipe_name);

        loop {
            match connect_and_write(&name, &payload) {
                Ok(()) => return Ok(()),
                Err(ConnectError::Busy) => {
                    // A previous client is mid-handshake. WaitNamedPipeW is the
                    // documented way to wait for an instance to free up, and it
                    // is bounded so a wedged client cannot hang the launcher.
                    // SAFETY: `name` is a live NUL-terminated buffer.
                    unsafe {
                        let _ = WaitNamedPipeW(PCWSTR(name.as_ptr()), 50);
                    }
                }
                Err(ConnectError::NotListening) => {
                    if Instant::now() >= deadline {
                        return Err(SingleInstanceError::PrimaryNotListening);
                    }
                    std::thread::sleep(RETRY_INTERVAL);
                }
                Err(ConnectError::Failed(code)) => {
                    return Err(SingleInstanceError::SystemFailure { code })
                }
            }
        }
    }
}

impl Drop for Win32SingleInstance {
    fn drop(&mut self) {
        // Best effort: `Drop` cannot report, and the kernel reclaims both the
        // mutex and the pipe when the process exits regardless.
        let _ = self.release();
    }
}

/// The listener thread and the event that stops it.
struct Listener {
    stop_event: SendHandle,
    join: Option<JoinHandle<()>>,
}

impl Listener {
    fn stop(mut self) {
        // SAFETY: `stop_event` is a live, uniquely owned event handle, and only
        // this owner ever sets it.
        unsafe {
            let _ = SetEvent(self.stop_event.get());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        // SAFETY: the thread has been joined, so it can no longer observe this
        // handle, and this is its only close.
        unsafe {
            let _ = CloseHandle(self.stop_event.get());
        }
    }
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// Why a connection attempt did not go through.
enum ConnectError {
    /// The pipe exists but every instance is busy. Retryable.
    Busy,
    /// No instance is listening yet. Retryable — this is the startup race.
    NotListening,
    /// Something else went wrong. Not retryable.
    Failed(u32),
}

fn connect_and_write(name: &[u16], payload: &[u8]) -> Result<(), ConnectError> {
    // SAFETY: `name` is a NUL-terminated UTF-16 buffer built by the caller and
    // outlives the call. OPEN_EXISTING never creates a pipe, so a wrong name
    // reports "not listening" instead of silently making a new one.
    // FILE_FLAG_OVERLAPPED is not optional: the pipe instance is in overlapped
    // mode, and a synchronous `WriteFile` on such a handle is a usage error.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    };

    let handle = match handle {
        Ok(handle) => handle,
        Err(e) => {
            let code = last_error_code(&e);
            return Err(if code == ERROR_FILE_NOT_FOUND.0 {
                ConnectError::NotListening
            } else if code == ERROR_PIPE_BUSY.0 {
                ConnectError::Busy
            } else {
                ConnectError::Failed(code)
            });
        }
    };

    let result = write_overlapped(handle, payload);
    // SAFETY: `handle` is live and owned here, and is closed exactly once on
    // every path out of this function.
    let closed = unsafe { CloseHandle(handle) };

    match (result, closed) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(code), _) => Err(ConnectError::Failed(code)),
        (Ok(()), Err(e)) => Err(ConnectError::Failed(last_error_code(&e))),
    }
}

/// Overlapped `WriteFile` that waits for its own completion.
fn write_overlapped(handle: HANDLE, payload: &[u8]) -> Result<(), u32> {
    // SAFETY: a null SECURITY_ATTRIBUTES means the default DACL.
    let event = match unsafe { CreateEventW(None, true, false, None) } {
        Ok(event) => event,
        Err(e) => return Err(last_error_code(&e)),
    };

    let mut overlapped = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };

    // SAFETY: `overlapped` is a live, owned OVERLAPPED with a valid event, and
    // its address outlives the call. `payload` is a live slice of the length
    // the kernel is told about. On an overlapped handle the byte count out of
    // WriteFile must be NULL, which is what `None` means here.
    let started = unsafe { WriteFile(handle, Some(payload), None, Some(&mut overlapped)) };

    let settled = match started {
        Ok(()) => Ok(()),
        Err(e) => {
            let code = last_error_code(&e);
            // ERROR_IO_PENDING is success: the write is under way and the event
            // will fire when it lands.
            if code == ERROR_IO_PENDING.0 {
                Ok(())
            } else {
                Err(code)
            }
        }
    };

    let outcome = match settled {
        Err(code) => Err(code),
        Ok(()) => {
            let mut written = 0u32;
            // SAFETY: `handle` is live and owned by the caller, and `overlapped`
            // is still owned here with its event. bWait = true blocks until the
            // kernel finishes, which is what a fire-and-forget client wants.
            let completed = unsafe { GetOverlappedResult(handle, &overlapped, &mut written, true) };
            match completed {
                Ok(()) if written == payload.len() as u32 => Ok(()),
                Ok(()) => Err(windows::Win32::Foundation::ERROR_WRITE_FAULT.0),
                Err(e) => Err(last_error_code(&e)),
            }
        }
    };

    // SAFETY: `event` is live and this is its only close.
    unsafe {
        let _ = CloseHandle(event);
    }
    outcome
}

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

/// Creates the pipe, announces readiness, and then serves until stopped.
fn listener_body(
    pipe_name: String,
    mut on_command: Box<dyn FnMut(InstanceCommand) + Send + 'static>,
    stop_event: SendHandle,
    ready: Sender<Result<(), SingleInstanceError>>,
) {
    let name = wide_nul(&pipe_name);

    // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives every use
    // below. FILE_FLAG_OVERLAPPED is what makes the listener stoppable: see the
    // module docs. The buffer sizes are advisory; 4 KiB is comfortably above the
    // largest frame this protocol sends.
    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            NAMED_PIPE_MODE(PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0),
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            None,
        )
    };
    // CreateNamedPipeW signals failure with INVALID_HANDLE_VALUE rather than a
    // null, which is the one API in this module that does it.
    if pipe == INVALID_HANDLE_VALUE || pipe.0.is_null() {
        let _ = ready.send(Err(SingleInstanceError::SystemFailure {
            code: unsafe { windows::Win32::Foundation::GetLastError() }.0,
        }));
        return;
    }

    if ready.send(Ok(())).is_err() {
        // The guard was dropped before it could use us. Tear down rather than
        // becoming a thread holding a pipe nobody can reach.
        // SAFETY: live and owned here, and this is the only close of each.
        unsafe {
            let _ = CloseHandle(pipe);
        }
        return;
    }

    serve(pipe, stop_event, &mut on_command);

    // `serve` owns the pipe for the length of the loop and hands it back
    // undeliberately: every early return inside it is a shutdown, and leaving
    // the instance open would let a later client write into a pipe nobody is
    // reading.
    // SAFETY: live and owned here, and this is the only close of it.
    unsafe {
        let _ = CloseHandle(pipe);
    }
}

/// Accepts connections and dispatches commands until the stop event fires.
fn serve(pipe: HANDLE, stop_event: SendHandle, on_command: &mut dyn FnMut(InstanceCommand)) {
    loop {
        // Each connection gets its own completion event, because an OVERLAPPED
        // may have exactly one outstanding operation at a time.
        // SAFETY: a null SECURITY_ATTRIBUTES means the default DACL.
        let event = match unsafe { CreateEventW(None, true, false, None) } {
            Ok(event) => event,
            Err(_) => return,
        };
        let mut connect_overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };

        // SAFETY: `pipe` is live and owned by the caller. `connect_overlapped`
        // is a live, owned OVERLAPPED whose event is valid, and its address
        // outlives the call.
        let already_connected =
            match unsafe { ConnectNamedPipe(pipe, Some(&mut connect_overlapped)) } {
                Ok(()) => true,
                Err(e) => match last_error_code(&e) {
                    // The normal case: nobody is connected yet, and the event will
                    // fire when somebody is.
                    code if code == ERROR_IO_PENDING.0 => false,
                    // ERROR_PIPE_CONNECTED: a client attached between the
                    // CreateNamedPipeW above and this call.
                    //
                    // ERROR_NO_DATA: a client attached, wrote, and hung up in the
                    // same window. This is not an error at all — it is a very fast
                    // client, which is exactly what a second launch is, and the
                    // bytes it wrote are still buffered. Treating it as a failure
                    // silently drops the message the user asked for.
                    code if code == ERROR_PIPE_CONNECTED.0 || code == ERROR_NO_DATA.0 => true,
                    _ => {
                        // SAFETY: live and owned here.
                        unsafe {
                            let _ = CloseHandle(event);
                        }
                        return;
                    }
                },
            };

        if !already_connected {
            // Wait on the stop event and the connect together, so a shutdown is
            // serviced even when no second launch has ever connected.
            // SAFETY: both handles are live, distinct, and owned for the
            // duration of the wait, and neither is closed while it is in flight.
            let waited =
                unsafe { WaitForMultipleObjects(&[stop_event.get(), event], false, INFINITE) }.0;

            if waited != 1 {
                // Stop signalled. Cancel the pending connect, then reap it;
                // closing the handle afterwards would also cancel it, but doing
                // it explicitly means the thread is not racing its own teardown.
                // SAFETY: `pipe` is live and owned by the caller;
                // `connect_overlapped` is still owned here.
                unsafe {
                    let _ = CancelIoEx(pipe, Some(&connect_overlapped));
                    let _ = CloseHandle(event);
                }
                return;
            }
            // The connect event fired. Settle the operation before reading,
            // because a cancelled operation signals the same event.
            //
            // The byte count is passed even though a connect transfers nothing:
            // MSDN marks this parameter optional, but passing null here faults
            // on this toolchain, and the count is free to supply.
            //
            // SAFETY: `pipe` is live, `connect_overlapped` is still owned here
            // with a valid event, and `transferred` is a live out-parameter.
            let mut transferred = 0u32;
            if unsafe { GetOverlappedResult(pipe, &connect_overlapped, &mut transferred, false) }
                .is_err()
            {
                // SAFETY: live and owned here.
                unsafe {
                    let _ = CloseHandle(event);
                }
                continue;
            }
        }

        // SAFETY: live and owned here.
        unsafe {
            let _ = CloseHandle(event);
        }

        if let Some(command) = read_frame(pipe, &stop_event) {
            on_command(command);
        }

        // SAFETY: `pipe` is live and owned by the caller. Failing to disconnect
        // is not fatal: the next ConnectNamedPipe recycles the instance.
        unsafe {
            let _ = DisconnectNamedPipe(pipe);
        }
    }
}

/// Reads one frame from a connected client, bounded and cancellable.
///
/// Returns `None` for a disconnect, a short read, an undecodable frame, or a
/// client that stayed silent too long. All four mean "nothing to act on", and
/// none is worth abandoning the listener over — a half-written frame from a
/// crashed client must not take the primary down with it.
fn read_frame(pipe: HANDLE, stop_event: &SendHandle) -> Option<InstanceCommand> {
    // SAFETY: a null SECURITY_ATTRIBUTES means the default DACL.
    let event = unsafe { CreateEventW(None, true, false, None) }.ok()?;

    let mut buffer = [0u8; 64];
    let mut read_overlapped = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };

    // SAFETY: `pipe` is live and owned by the caller. `read_overlapped` is a
    // live, owned OVERLAPPED with a valid event and its address outlives the
    // call. `buffer` is a live slice whose length is passed to the kernel.
    let started = unsafe {
        ReadFile(
            pipe,
            Some(&mut buffer[..]),
            None,
            Some(&mut read_overlapped),
        )
    };

    if let Err(e) = started {
        let code = last_error_code(&e);
        if code != ERROR_IO_PENDING.0 {
            // SAFETY: live and owned here.
            unsafe {
                let _ = CloseHandle(event);
            }
            return None;
        }
    }

    // SAFETY: both handles are live, distinct, and owned for the duration of
    // the wait. The timeout is what stops one silent client from wedging the
    // listener.
    let waited = unsafe {
        WaitForMultipleObjects(&[stop_event.get(), event], false, CLIENT_READ_TIMEOUT_MS)
    }
    .0;

    let frame = if waited == 1 {
        let mut read = 0u32;
        // SAFETY: `pipe` is live, and `read_overlapped` is still owned here with
        // a valid event. bWait = false because the event already fired.
        let completed = unsafe { GetOverlappedResult(pipe, &read_overlapped, &mut read, false) };
        match completed {
            Ok(()) => InstanceCommand::decode(&buffer[..read as usize]),
            Err(_) => None,
        }
    } else {
        // Stopped, or the client stayed silent. Cancel the read so the thread
        // is not left with an operation in flight when it closes the handle.
        // SAFETY: `pipe` is live and owned by the caller; `read_overlapped` is
        // still owned here.
        unsafe {
            let _ = CancelIoEx(pipe, Some(&read_overlapped));
        }
        None
    };

    // SAFETY: live and owned here, and this is its only close.
    unsafe {
        let _ = CloseHandle(event);
    }
    frame
}

#[cfg(test)]
/// A suffix unique to this process and thread, so tests never share a named
/// object. A named object is global, and a fixed name would let one test's
/// mutex decide another test's answer.
pub(crate) fn unique_suffix(tag: &str) -> String {
    // SAFETY: takes no arguments and reads no memory that could be invalid.
    let pid = std::process::id();
    format!("{tag}-{pid}-{:?}", std::thread::current().id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_show_command_as_a_versioned_fixed_header() {
        let bytes = InstanceCommand::Show.encode();
        assert_eq!(bytes.len(), 4);
        assert_eq!(bytes[0], InstanceCommand::SHOW);
        assert_eq!(bytes[1], InstanceCommand::VERSION);
    }

    #[test]
    fn round_trips_a_command_through_the_wire_format() {
        let bytes = InstanceCommand::Show.encode();
        assert_eq!(InstanceCommand::decode(&bytes), Some(InstanceCommand::Show));
    }

    #[test]
    fn refuses_a_truncated_frame_rather_than_guessing() {
        // A one-byte frame would decode as "Show" and raise the window for a
        // message that never fully arrived.
        assert_eq!(InstanceCommand::decode(&[]), None);
        assert_eq!(InstanceCommand::decode(&[InstanceCommand::SHOW]), None);
        assert_eq!(InstanceCommand::decode(&[InstanceCommand::SHOW, 1]), None);
    }

    #[test]
    fn refuses_a_frame_from_a_future_version() {
        let mut bytes = InstanceCommand::Show.encode();
        bytes[1] = InstanceCommand::VERSION + 1;
        assert_eq!(InstanceCommand::decode(&bytes), None);
    }

    #[test]
    fn refuses_an_unknown_opcode() {
        let mut bytes = InstanceCommand::Show.encode();
        bytes[0] = 0x7F;
        assert_eq!(InstanceCommand::decode(&bytes), None);
    }

    #[test]
    fn a_guard_claims_nothing_until_asked() {
        let guard = Win32SingleInstance::with_names("never-used-mutex", "never-used-pipe");
        assert!(!guard.is_primary());
    }

    #[test]
    fn release_without_holding_the_role_is_a_no_op() {
        let mut guard = Win32SingleInstance::with_names("never-used-mutex", "never-used-pipe");
        // Not an error: "not primary" is a normal state between launches.
        guard.release().expect("releasing nothing is fine");
        assert!(!guard.is_primary());
    }

    #[test]
    fn the_first_acquirer_is_primary_and_the_second_is_not() {
        let suffix = unique_suffix("acquire");
        let (mutex, pipe) = (
            format!("Local\\test-{suffix}"),
            format!("\\\\.\\pipe\\test-{suffix}"),
        );

        let mut first = Win32SingleInstance::with_names(&mutex, &pipe);
        assert!(first.acquire().expect("first acquire"), "first must win");
        assert!(first.is_primary());

        let mut second = Win32SingleInstance::with_names(&mutex, &pipe);
        assert!(
            !second.acquire().expect("second acquire"),
            "second must lose"
        );
        assert!(!second.is_primary());

        first.release().expect("release");
        assert!(!first.is_primary());
    }

    #[test]
    fn acquiring_after_release_succeeds_again() {
        // The crash-recovery path: the kernel drops the mutex when the last
        // handle closes, so a relaunch is never permanently locked out.
        let suffix = unique_suffix("reacquire");
        let (mutex, pipe) = (
            format!("Local\\test-{suffix}"),
            format!("\\\\.\\pipe\\test-{suffix}"),
        );

        let mut guard = Win32SingleInstance::with_names(&mutex, &pipe);
        assert!(guard.acquire().expect("first"));
        guard.release().expect("release");

        let mut again = Win32SingleInstance::with_names(&mutex, &pipe);
        assert!(again.acquire().expect("acquire after release"));
        again.release().expect("release");
    }

    #[test]
    fn a_command_reaches_the_primary_through_the_pipe() {
        let suffix = unique_suffix("roundtrip");
        let (mutex, pipe) = (
            format!("Local\\test-{suffix}"),
            format!("\\\\.\\pipe\\test-{suffix}"),
        );

        let mut primary = Win32SingleInstance::with_names(&mutex, &pipe);
        assert!(primary.acquire().expect("acquire"));

        let (tx, rx) = mpsc::channel::<InstanceCommand>();
        primary
            .start_listener(Box::new(move |command| {
                let _ = tx.send(command);
            }))
            .expect("listener starts");

        // A second launch, simulated in-process: it did not win the mutex, so it
        // sends "show" and exits.
        let secondary = Win32SingleInstance::with_names(&mutex, &pipe);
        assert!(!secondary.is_primary());
        secondary
            .notify_primary(InstanceCommand::Show, Duration::from_secs(10))
            .expect("notify");

        let received = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("primary should have received the command");
        assert_eq!(received, InstanceCommand::Show);

        primary.stop_listener();
        primary.release().expect("release");
    }

    #[test]
    fn a_second_command_is_also_delivered() {
        // The listener must recycle the pipe instance, not serve one client and
        // stop; a launcher gets launched repeatedly.
        let suffix = unique_suffix("twice");
        let (mutex, pipe) = (
            format!("Local\\test-{suffix}"),
            format!("\\\\.\\pipe\\test-{suffix}"),
        );

        let mut primary = Win32SingleInstance::with_names(&mutex, &pipe);
        assert!(primary.acquire().expect("acquire"));

        let (tx, rx) = mpsc::channel::<InstanceCommand>();
        primary
            .start_listener(Box::new(move |command| {
                let _ = tx.send(command);
            }))
            .expect("listener starts");

        let secondary = Win32SingleInstance::with_names(&mutex, &pipe);
        for _ in 0..2 {
            secondary
                .notify_primary(InstanceCommand::Show, Duration::from_secs(10))
                .expect("notify");
            let received = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("command should arrive");
            assert_eq!(received, InstanceCommand::Show);
        }

        primary.stop_listener();
        primary.release().expect("release");
    }

    #[test]
    fn stopping_the_listener_does_not_block_on_a_pending_connect() {
        // The reason the pipe is overlapped: with a blocking connect, this
        // teardown would hang forever when no second launch ever arrives.
        let suffix = unique_suffix("stopidle");
        let (mutex, pipe) = (
            format!("Local\\test-{suffix}"),
            format!("\\\\.\\pipe\\test-{suffix}"),
        );

        let mut primary = Win32SingleInstance::with_names(&mutex, &pipe);
        assert!(primary.acquire().expect("acquire"));
        primary
            .start_listener(Box::new(|_| {}))
            .expect("listener starts");

        let started = Instant::now();
        primary.stop_listener();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stop_listener must interrupt the pending connect"
        );

        primary.release().expect("release");
    }

    #[test]
    fn notifying_a_pipe_that_never_comes_up_times_out_rather_than_hanging() {
        // The startup race taken to its worst case: the mutex is held by
        // something that never opened its pipe. The launcher must still get an
        // answer promptly, and the answer must be distinguishable from
        // "nothing is running".
        let suffix = unique_suffix("nolistener");
        let (mutex, pipe) = (
            format!("Local\\test-{suffix}"),
            format!("\\\\.\\pipe\\test-{suffix}"),
        );

        let mut primary = Win32SingleInstance::with_names(&mutex, &pipe);
        assert!(primary.acquire().expect("acquire"));
        // Deliberately no listener.

        let secondary = Win32SingleInstance::with_names(&mutex, &pipe);
        let started = Instant::now();
        let result = secondary.notify_primary(InstanceCommand::Show, Duration::from_millis(150));

        assert_eq!(result, Err(SingleInstanceError::PrimaryNotListening));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must respect the deadline rather than block indefinitely"
        );

        primary.release().expect("release");
    }

    #[test]
    fn start_listener_is_refused_when_not_primary() {
        let mut guard = Win32SingleInstance::with_names("never-used-mutex", "never-used-pipe");
        let error = guard
            .start_listener(Box::new(|_| {}))
            .expect_err("only the primary listens");
        assert_eq!(error, SingleInstanceError::AlreadyRunning);
    }

    #[test]
    fn last_error_code_unpacks_a_packed_win32_code() {
        let error = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(5));
        assert_eq!(last_error_code(&error), 5);
    }

    #[test]
    fn last_error_code_passes_a_non_win32_hresult_through() {
        let error =
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8004_0000u32 as i32));
        assert_eq!(last_error_code(&error), 0x8004_0000);
    }
}
