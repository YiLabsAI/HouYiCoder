//! The process table probe on Windows.

use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE, WIN32_ERROR};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// True when a process with this identifier exists.
///
/// A refusal to open the process still means it exists, because the query
/// access is what this account lacks; any other open failure means no process
/// holds that identifier. A process that already exited does not count.
#[expect(unsafe_code, reason = "queries the process table")]
pub fn alive(pid: i32) -> bool {
    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) }
    {
        Ok(handle) => handle,
        Err(e) => return WIN32_ERROR::from_error(&e) == Some(ERROR_ACCESS_DENIED),
    };
    let mut status = 0u32;
    let queried = unsafe { GetExitCodeProcess(process, &mut status) };
    drop(unsafe { CloseHandle(process) });
    queried.is_ok() && status as i32 == STILL_ACTIVE.0
}
