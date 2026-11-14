//! The kernel calls behind the native Windows launcher.
//!
//! A command enters its job at creation: the start-up attribute list carries
//! the job and the exact handles the command may inherit, so the process runs
//! inside the fence from its first instruction and no unrelated handle reaches
//! it. Every call here wraps one system call; the safe launcher composes them.
//! A command starts here instead of at the launcher port because this crate
//! sits below that port; the request that reaches this module is already the
//! sandbox session's decision on whether the command is fenced.

#![expect(
    unsafe_code,
    reason = "the launcher's kernel calls take handles and raw pointers"
)]

use std::ffi::c_void;
use std::io::{Error, Result};
use std::iter::once;
use std::mem::ManuallyDrop;
use std::mem::size_of;
use std::mem::size_of_val;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::ptr::null_mut;
use std::sync::Mutex;

use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Memory::{GetProcessHeap, HEAP_FLAGS, HeapAlloc, HeapFree};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
    RegisterWaitForSingleObject, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UnregisterWaitEx, UpdateProcThreadAttribute, WT_EXECUTEONLYONCE,
};
use windows::core::{PCWSTR, PWSTR};

/// Create a job whose processes die when its last handle closes, so a command
/// tree cannot outlive the call that started it.
pub(super) fn create_job() -> Result<OwnedHandle> {
    let job = HeldHandle(unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(io_error)?);
    apply_job_limits(job.0)?;
    Ok(adopt(job))
}

/// Apply the one limit the launcher relies on. The kill-on-close flag is what
/// makes the handle itself the lifetime of the tree.
fn apply_job_limits(job: HANDLE) -> Result<()> {
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let length = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
    let class = JobObjectExtendedLimitInformation;
    unsafe { SetInformationJobObject(job, class, &info as *const _ as *const c_void, length) }
        .map_err(io_error)
}

/// Open the write end of a named pipe instance for the command to inherit.
/// Sharing is refused, so no second handle to the same stream exists, and the
/// open connects to the instance the caller already created.
pub(super) fn open_pipe_write_end(name: &str) -> Result<OwnedHandle> {
    let attributes = inheritable();
    let name = wide(name);
    let opened = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            Some(&attributes),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(io_error)?;
    let opened = HeldHandle(opened);
    Ok(adopt(opened))
}

/// Open the null device as the command's input, so a command that reads input
/// reaches end of file at once instead of waiting on the host's terminal.
pub(super) fn null_input() -> Result<OwnedHandle> {
    let attributes = inheritable();
    let name = wide("NUL");
    let opened = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&attributes),
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(io_error)?;
    let opened = HeldHandle(opened);
    Ok(adopt(opened))
}

/// Start a command with its output on the given write ends, inside the given
/// job when one is supplied. The system duplicates the write ends into the
/// child, so the caller closes its own copies afterwards and the read ends
/// observe end of stream when the command and its descendants exit.
pub(super) fn spawn(
    command_line: &str,
    cwd: &Path,
    job: Option<RawHandle>,
    stdio: [RawHandle; 3],
) -> Result<OwnedHandle> {
    let mut command: Vec<u16> = wide(command_line);
    let directory: Vec<u16> = cwd.as_os_str().encode_wide().chain(once(0)).collect();
    let attribute_list = AttributeList::new(job, stdio)?;
    let [input, output, error_stream] = stdio.map(as_handle);
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = input;
    startup.StartupInfo.hStdOutput = output;
    startup.StartupInfo.hStdError = error_stream;
    startup.lpAttributeList = attribute_list.list;
    let mut information = PROCESS_INFORMATION::default();
    let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW;
    unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            true,
            flags,
            None,
            PCWSTR(directory.as_ptr()),
            &startup.StartupInfo,
            &mut information,
        )
    }
    .map_err(io_error)?;
    let process = HeldHandle(information.hProcess);
    // The thread handle is not needed once the command runs; holding it open
    // keeps the thread object alive for no one.
    let thread = HeldHandle(information.hThread);
    drop(thread);
    Ok(adopt(process))
}

/// A wait registered on a process handle that reports the exit code once,
/// from the system's thread pool. No thread of this process parks on the
/// command, so awaiting a long command costs no worker.
///
/// The handle and the callback context are held as addresses rather than as
/// pointer types, because the wait itself moves between threads while the
/// system calls back on a thread of its own. Soundness rests on the removal
/// below: the blocking form of unregistration returns only once no callback
/// can still reach the context, so freeing it afterwards is safe.
pub(super) struct ExitWait {
    wait: usize,
    context: usize,
}

/// The one receiver waiting for a command's exit code.
type ExitReport = Box<dyn FnOnce(u32) + Send>;

/// What the callback reports through: the process to read the code from and
/// the one receiver waiting for it.
struct ExitContext {
    process: usize,
    wake: Mutex<Option<ExitReport>>,
}

impl ExitWait {
    /// Register a wait on the process that calls the given closure with its
    /// exit code once the process ends.
    pub(super) fn register(process: RawHandle, wake: ExitReport) -> Result<Self> {
        let context = Box::into_raw(Box::new(ExitContext {
            process: process as usize,
            wake: Mutex::new(Some(wake)),
        }));
        let mut wait = HANDLE::default();
        let registered = unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                as_handle(process),
                Some(exit_reached),
                Some(context.cast::<c_void>() as *const c_void),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };
        match registered {
            Ok(()) => Ok(Self {
                wait: wait.0 as usize,
                context: context as usize,
            }),
            Err(error) => {
                // The context never reached the system, so this is the only
                // owner and the callback cannot run.
                drop(unsafe { Box::from_raw(context) });
                Err(io_error(error))
            }
        }
    }
}

impl Drop for ExitWait {
    fn drop(&mut self) {
        unsafe {
            let wait = as_handle(self.wait as RawHandle);
            // The blocking form returns once no callback can still run and
            // closes the wait handle itself, so neither is closed here. It is
            // a short system call on this thread, bounded by one callback
            // that only takes a mutex and sends on a channel.
            let removed = UnregisterWaitEx(wait, Some(INVALID_HANDLE_VALUE));
            if removed.is_err() {
                // A wait that did not unregister may still call back into the
                // context, so the wait and its context are leaked on purpose
                // rather than freed under a running callback. This only
                // happens for a handle the system already considers invalid.
                tracing::warn!("the command exit wait did not unregister");
                return;
            }
            drop(Box::from_raw(self.context as *mut ExitContext));
        }
    }
}

/// The callback the thread pool runs once the process ends. A failed exit
/// code read reports nothing, and the receiver sees its channel closed.
///
/// The context is the value ExitWait registered, and ExitWait frees it only
/// after the blocking unregistration, so it outlives this call.
unsafe extern "system" fn exit_reached(context: *mut c_void, _fired: bool) {
    let context = unsafe { &*context.cast::<ExitContext>() };
    let process = as_handle(context.process as RawHandle);
    let wake = context
        .wake
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let mut code = 0u32;
    let read = unsafe { GetExitCodeProcess(process, &mut code) };
    if read.is_err() {
        // Dropping the closure closes the channel, so the waiting caller
        // learns the report was lost instead of awaiting a wait that already
        // fired and will never report again.
        return;
    }
    if let Some(wake) = wake {
        wake(code);
    }
}

/// End one process, used when no job groups the tree. A refusal is reported:
/// the caller just decided the command has to stop, and a stop that did not
/// happen is what an operator needs to see.
pub(super) fn terminate_process(process: RawHandle) {
    if let Err(error) = unsafe { TerminateProcess(as_handle(process), 1) } {
        tracing::warn!(%error, "the launcher could not end the command process");
    }
}

/// End every process in a job. The system walks the whole tree, so a
/// grandchild that outlived its parent is included.
pub(super) fn terminate_job(job: RawHandle) {
    if let Err(error) = unsafe { TerminateJobObject(as_handle(job), 1) } {
        tracing::warn!(%error, "the launcher could not end the command tree");
    }
}

/// The attribute list a command starts under: the job it enters at creation
/// and the only handles it may inherit. The system records the address of a
/// value rather than a copy and reads it again when the process starts and
/// when the list is deleted, so every value lives in a heap allocation this
/// type owns: an inline array would move with this struct and leave the list
/// pointing at the bytes the struct used to occupy. The allocations drop
/// after the drop body below, which is the order the system needs.
struct AttributeList {
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
    heap: HANDLE,
    initialized: bool,
    job_handles: Option<Box<[HANDLE]>>,
    stdio_handles: Box<[HANDLE]>,
}

impl AttributeList {
    fn new(job: Option<RawHandle>, stdio: [RawHandle; 3]) -> Result<Self> {
        let mut size = 0usize;
        let count = 2;
        // The first call reports the size the list needs and is expected to
        // answer an error, so only the size it wrote matters.
        let _expected_failure =
            unsafe { InitializeProcThreadAttributeList(None, count, None, &mut size) };
        if size == 0 {
            return Err(Error::last_os_error());
        }
        let heap = unsafe { GetProcessHeap() }.map_err(io_error)?;
        let raw = unsafe { HeapAlloc(heap, HEAP_FLAGS(0), size) };
        if raw.is_null() {
            return Err(Error::other("the attribute list allocation failed"));
        }
        let mut list = Self {
            list: LPPROC_THREAD_ATTRIBUTE_LIST(raw),
            heap,
            initialized: false,
            job_handles: None,
            stdio_handles: Vec::from(stdio.map(as_handle)).into_boxed_slice(),
        };
        unsafe { InitializeProcThreadAttributeList(Some(list.list), count, None, &mut size) }
            .map_err(io_error)?;
        list.initialized = true;
        // Each buffer is owned before the call that records its address, so a
        // failed call still leaves the list referring to memory this type
        // holds until the list is deleted.
        if let Some(job) = job {
            list.job_handles = Some(vec![as_handle(job)].into_boxed_slice());
            let handles = list.job_handles.as_deref().unwrap_or_default();
            set_attribute(list.list, PROC_THREAD_ATTRIBUTE_JOB_LIST, handles)?;
        }
        set_attribute(
            list.list,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            &list.stdio_handles,
        )?;
        Ok(list)
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            // A list whose initialisation failed was never registered with
            // the system, so only the allocation is released.
            if self.initialized {
                DeleteProcThreadAttributeList(self.list);
            }
            let _freed = HeapFree(self.heap, HEAP_FLAGS(0), Some(self.list.0));
        }
    }
}

/// Record one attribute on the list. The system keeps the pointer rather than
/// a copy, so the buffer must stay alive and unmoved until the list is
/// deleted.
fn set_attribute(
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
    attribute: u32,
    values: &[HANDLE],
) -> Result<()> {
    let bytes = size_of_val(values);
    unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            attribute as usize,
            Some(values.as_ptr() as *const c_void),
            bytes,
            None,
            None,
        )
    }
    .map_err(io_error)
}

/// A kernel handle closed on every path out unless it is adopted into an
/// owned handle first.
struct HeldHandle(HANDLE);

impl Drop for HeldHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _closed = unsafe { CloseHandle(self.0) };
            self.0 = HANDLE(null_mut());
        }
    }
}

/// Hand the guard's handle to an owned handle. The guard must not close it
/// afterwards, so its drop is cancelled rather than run.
fn adopt(held: HeldHandle) -> OwnedHandle {
    let held = ManuallyDrop::new(held);
    unsafe { OwnedHandle::from_raw_handle(held.0.0.cast()) }
}

fn as_handle(raw: RawHandle) -> HANDLE {
    HANDLE(raw)
}

fn inheritable() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: true.into(),
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(once(0)).collect()
}

fn io_error(error: windows::core::Error) -> Error {
    Error::other(error)
}
