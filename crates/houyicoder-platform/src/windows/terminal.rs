//! Windows console boundary: the mode owner for one session, the control
//! events that end it, and the close answer a loop polls. The guard is the
//! single active owner of the input and output console modes; a second
//! concurrent owner is refused because the modes are process-global. The
//! control handler raises the shared flag and the wake, then holds its
//! return open for a bounded window so the loop's teardown can run. The
//! wait never touches the output handle, which can block on a closing
//! console.

use std::io::Error as IoError;
use std::slice::from_mut;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::yield_now;
use std::time::{Duration, Instant};

use crate::desktop::terminal::vt::VtEvent;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    CONSOLE_MODE, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, GetConsoleMode,
    GetStdHandle, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler, SetConsoleMode,
};
use windows::core::BOOL;
use windows::core::Error;
use windows::core::HRESULT;
use windows::core::Result;

/// How long the control handler holds its return after a close event, so a
/// loop that is awake can run its teardown before the process ends.
const CLOSE_WAIT: Duration = Duration::from_millis(400);

/// The quick-edit selection bit, cleared with the extended flag set so a
/// click cannot freeze the reader.
const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;

/// The console mode pair a guard holds, put back on release.
struct ConsoleModes {
    input: u32,
    output: u32,
}

/// The process-wide guard state: the saved modes and the active guard.
static GUARD: Mutex<Option<ActiveGuard>> = Mutex::new(None);

/// The state one active guard carries.
struct ActiveGuard {
    modes: ConsoleModes,
}

/// The flag a close-class event raises and the loop reads. The same flag the
/// watch hands in is installed here so the handler writes it directly, which
/// keeps the loop's poll and the handler's raise on one store.
static CLOSED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// The requested flag the handler raises when a close-class event arrives.
/// Held in its own static because the handler is a plain function with no
/// captured state: the watch passes its flag once, at registration.
static REQUESTED_FOR_HANDLER: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// The wake a close event raises, handed in at registration.
static WAKE: Mutex<Option<Box<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Whether the console has reported a close, logoff, or shutdown. The
/// handler raises the flag before the process is taken, so a loop polling
/// between events sees it.
pub(crate) fn console_gone() -> bool {
    CLOSED
        .get()
        .is_some_and(|closed| closed.load(Ordering::SeqCst))
}

/// Register the console control handler. A session switch runs the loop
/// again against the same registration, so the second call only refreshes
/// the wake.
pub(crate) fn watch_close_event(
    requested: Arc<AtomicBool>,
    wake: impl Fn() + Send + Sync + 'static,
) -> std::io::Result<()> {
    let closed = CLOSED.get_or_init(|| Arc::new(AtomicBool::new(false)));
    *WAKE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(wake));
    register_handler(requested, Arc::clone(closed)).map_err(|e| IoError::other(e.to_string()))
}

/// Whether the process's input is a pipe rather than a console. A terminal
/// window that hands the process a named pipe answers no console API, so
/// the event reader has to decode the byte stream the pipe carries.
#[expect(
    unsafe_code,
    reason = "reads the input handle type through the console API"
)]
pub(crate) fn input_is_pipe() -> bool {
    let handle = match unsafe { GetStdHandle(STD_INPUT_HANDLE) } {
        Ok(handle) => handle,
        Err(_) => return false,
    };
    let mut mode = CONSOLE_MODE(0);
    // A console handle answers the mode query; a pipe does not, which is
    // the distinction a pipe-mode terminal rides on.
    unsafe { GetConsoleMode(handle, &mut mode) }.is_err()
}

/// Read one console input record and map it to a decoded event. The control
/// keys the console reports with a zero virtual key code -- the ones a
/// ConPTY bridge delivers for the control block -- are mapped to their
/// letters here, so a Ctrl+O is not lost to the layout's failure to
/// resolve it through the keyboard layout table.
#[expect(unsafe_code, reason = "reads console input records through the API")]
pub fn read_console_event() -> Option<VtEvent> {
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.ok()?;
    let mut record: INPUT_RECORD = INPUT_RECORD::default();
    let buf = from_mut(&mut record);
    unsafe { ReadConsoleInputW(handle, buf, &mut 0) }.ok()?;
    if record.EventType as u32 != KEY_EVENT {
        return None;
    }
    let key = unsafe { record.Event.KeyEvent };
    if !key.bKeyDown.as_bool() {
        return None;
    }
    let uchar = unsafe { key.uChar.UnicodeChar };
    let vk = key.wVirtualKeyCode;
    // The control block: a byte in 0x01..=0x1A with no virtual key is the
    // control form. The console layout resolver returns zero for these under
    // ConPTY, so crossterm drops them; map them here instead. Tab (0x09),
    // Enter (0x0D), Backspace (0x08) and Esc (0x1B) have their own virtual
    // keys and arrive as normal events, so they are left to the caller.
    if (0x01..=0x1A).contains(&uchar) && vk == 0 {
        let letter = (uchar + (b'a' as u16) - 1) as u8 as char;
        return Some(VtEvent::Ctrl(letter));
    }
    None
}

/// The handler the console calls with no context; the flags it writes are
/// the shared statics above.
#[expect(
    unsafe_code,
    reason = "the console calls this entry point through its control chain"
)]
unsafe extern "system" fn control_handler(event: u32) -> BOOL {
    if matches!(
        event,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
    ) {
        let deadline = Instant::now() + CLOSE_WAIT;
        raise_close();
        // Hold the return open for the bounded window so the loop's
        // teardown can run. The wait reads the guard and the wake only,
        // never the output handle.
        spin_until_released(deadline);
        // Answered as handled: the process asked to end, and the default
        // policy would end it before the teardown runs.
        return BOOL::from(true);
    }
    // Other events pass on so the default policy still sees them.
    BOOL::from(false)
}

/// Raise the close flag and the wake. The requested flag the loop polls is
/// the same one the handler stores to, so the loop sees a close-class event
/// within one poll interval.
fn raise_close() {
    if let Some(closed) = CLOSED.get() {
        closed.store(true, Ordering::SeqCst);
    }
    if let Some(requested) = REQUESTED_FOR_HANDLER.get() {
        requested.store(true, Ordering::SeqCst);
    }
    let wake = WAKE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(wake) = wake.as_ref() {
        wake();
    }
}

/// Wait for the active guard to release, up to the deadline. A yield-based
/// wait keeps the handler free of any lock leave could hold.
fn spin_until_released(deadline: Instant) {
    while Instant::now() < deadline {
        let released = GUARD.lock().unwrap_or_else(|e| e.into_inner()).is_none();
        if released {
            return;
        }
        yield_now();
    }
}

/// The one console mode owner. A second concurrent owner is refused because
/// the modes are process-global.
pub(crate) struct ConsoleGuard {
    released: bool,
}

impl ConsoleGuard {
    /// Take the console modes and clear the quick-edit selection flag.
    /// A second concurrent owner is an error naming the conflict, because
    /// two owners would each restore what the other still needs.
    pub(crate) fn acquire() -> Result<Self> {
        let mut guard = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Err(Error::new(HRESULT(-1), "another console guard is active"));
        }
        let (input_handle, output_handle) = std_handles()?;
        let input_mode = read_mode(input_handle)?;
        let output_mode = read_mode(output_handle)?;
        // The extended flag must be set for the quick-edit clear to hold:
        // the pair is defined by the console API as one mode write.
        set_mode(
            input_handle,
            (input_mode & !ENABLE_QUICK_EDIT_MODE) | ENABLE_EXTENDED_FLAGS,
        )?;
        *guard = Some(ActiveGuard {
            modes: ConsoleModes {
                input: input_mode,
                output: output_mode,
            },
        });
        Ok(Self { released: false })
    }

    /// Restore the modes the console started with. Idempotent with drop.
    pub(crate) fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let taken = GUARD.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(active) = taken {
            restore_modes(&active.modes);
        }
    }
}

impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Restore both console modes. Best effort: the console may already be
/// closing, and a restore failure must not panic on the way out.
fn restore_modes(modes: &ConsoleModes) {
    let Ok((input_handle, output_handle)) = std_handles() else {
        return;
    };
    drop(set_mode(input_handle, modes.input));
    drop(set_mode(output_handle, modes.output));
}

/// The standard input and output handles. Errors when the process has no
/// console, which a pipe-attached process may not.
#[expect(
    unsafe_code,
    reason = "reads the process handles through the console API"
)]
fn std_handles() -> Result<(HANDLE, HANDLE)> {
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) }?;
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }?;
    Ok((input, output))
}

#[expect(unsafe_code, reason = "reads the console mode through the console API")]
fn read_mode(handle: HANDLE) -> Result<u32> {
    let mut mode = CONSOLE_MODE(0);
    unsafe { GetConsoleMode(handle, &mut mode) }?;
    Ok(mode.0)
}

#[expect(
    unsafe_code,
    reason = "writes the console mode through the console API"
)]
fn set_mode(handle: HANDLE, mode: u32) -> Result<()> {
    unsafe { SetConsoleMode(handle, CONSOLE_MODE(mode)) }
}

/// Register the console control handler once for the process. The registration
/// outlives any one session, which is correct for a process that serves
/// one session at a time. The requested flag and the closed flag share one
/// store: a close-class event sets both through the handler, so the loop
/// reads the same flag it polls.
#[expect(unsafe_code, reason = "installs the handler the console calls")]
fn register_handler(requested: Arc<AtomicBool>, closed: Arc<AtomicBool>) -> Result<()> {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    if REGISTERED.get().is_some() {
        return Ok(());
    }
    // The requested flag the loop reads is the same flag the handler raises,
    // published before the handler is installed so a close event after
    // registration sets it.
    REQUESTED_FOR_HANDLER.set(Arc::clone(&requested)).ok();
    unsafe { SetConsoleCtrlHandler(Some(control_handler), true) }?;
    let _ = REGISTERED.set(());
    // Keep the closed flag reachable so console_gone still reports after
    // the request flag is moved into the handler's static.
    drop(closed);
    Ok(())
}
