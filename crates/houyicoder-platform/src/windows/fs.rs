//! The Windows atomic file replace: write a uniquely-named temp beside the
//! target, then rename it over it with POSIX semantics, retrying a sharing
//! violation through a backoff. The lexical guard, the temp name, and the
//! backoff come from the host layer; this is the thin unsafe shell.

use std::ffi::c_void;
use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::iter::once;
use std::path::Path;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;

use crate::host::fs::{LexicalRejection, backoff_schedule, reject, temp_name_for};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, DELETE, DeleteFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FileRenameInfoEx, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, MoveFileExW, SetFileInformationByHandle, WriteFile,
};
use windows::core::{HRESULT, PCWSTR};

/// The per-writer counter that names one temp file after another, so two
/// replaces in the same process never share the temp path one truncates while
/// the other writes.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Replace the target file's contents atomically.
///
/// A uniquely-named temp file is written beside the target and renamed over it
/// with POSIX semantics, so a reader holding the target open does not block the
/// replace and a half-written file is never what a reader sees. A filesystem
/// that does not understand the POSIX flag falls back to a path-based move, and
/// a sharing violation either path meets retries through the backoff schedule
/// before the call gives up. A failed replace deletes the temp, so no transient
/// file is left behind.
#[expect(
    unsafe_code,
    reason = "creates, writes, renames, and deletes the temp through the kernel API"
)]
pub fn atomic_replace(target: &Path, bytes: &[u8]) -> IoResult<()> {
    let target_text = target.to_string_lossy();
    if let Some(reason) = reject(&target_text) {
        return Err(IoError::other(lexical_message(reason)));
    }
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let temp_name = temp_name_for(&file_name, id(), seq);
    let temp_path = parent.join(&temp_name);
    let temp_wide = wide(&temp_path.to_string_lossy());
    // The no-null form is for the POSIX rename, which carries a length. The
    // path-based fallback takes null-terminated strings, so it builds its own.
    let target_wide: Vec<u16> = target_text.encode_utf16().collect();
    // The handle carries DELETE so the POSIX rename can act on it, and the
    // share-delete so a path-based move in the fallback is not blocked by it.
    let temp = create_temp(&temp_wide)?;
    let mut handle_open = true;
    let outcome = write_all(temp, bytes).and_then(|()| match posix_rename(temp, &target_wide) {
        Ok(()) => Ok(()),
        Err(RenameError::Unsupported) => {
            // A filesystem without the POSIX flag cannot rename through the
            // handle. Close it so the path-based move is not held by it, then
            // move with null-terminated paths.
            drop(unsafe { CloseHandle(temp) });
            handle_open = false;
            let target_nul = wide(&target_text);
            move_file(&temp_wide, &target_nul)
        }
        Err(RenameError::Failed(error)) => Err(error),
    });
    if handle_open {
        drop(unsafe { CloseHandle(temp) });
    }
    if outcome.is_err() {
        drop(delete_file(&temp_wide));
    }
    outcome
}

/// Create the temp file, opened for write and delete and shared for read and
/// delete, so the rename and the fallback move both reach it.
#[expect(unsafe_code, reason = "creates the temp through the kernel API")]
fn create_temp(temp_wide: &[u16]) -> IoResult<HANDLE> {
    let handle = unsafe {
        CreateFileW(
            PCWSTR(temp_wide.as_ptr()),
            FILE_GENERIC_WRITE.0 | DELETE.0,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            None,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    handle.map_err(|e| IoError::other(e.to_string()))
}

/// Write the full payload to the temp handle.
#[expect(unsafe_code, reason = "writes the temp through the kernel API")]
fn write_all(temp: HANDLE, bytes: &[u8]) -> IoResult<()> {
    unsafe { WriteFile(temp, Some(bytes), None, None) }.map_err(|e| IoError::other(e.to_string()))
}

/// Rename the open temp handle over the target through the POSIX-semantics
/// flag, retrying a sharing violation through the backoff schedule. A
/// filesystem that does not understand the flag answers with a parameter or
/// not-supported error, which the caller turns into a path-based fallback.
#[expect(
    unsafe_code,
    reason = "renames the temp over the target through the kernel API"
)]
fn posix_rename(temp: HANDLE, target_wide: &[u16]) -> Result<(), RenameError> {
    let info = build_rename_info(target_wide);
    let sharing = HRESULT::from_win32(ERROR_SHARING_VIOLATION.0);
    let unsupported = [
        HRESULT::from_win32(ERROR_INVALID_PARAMETER.0),
        HRESULT::from_win32(ERROR_NOT_SUPPORTED.0),
    ];
    for step in backoff_schedule() {
        match unsafe {
            SetFileInformationByHandle(
                temp,
                FileRenameInfoEx,
                info.as_ptr() as *const c_void,
                info.len() as u32,
            )
        } {
            Ok(()) => return Ok(()),
            Err(error) if error.code() == sharing => {
                sleep(step);
                continue;
            }
            Err(error) if unsupported.contains(&error.code()) => {
                return Err(RenameError::Unsupported);
            }
            Err(error) => return Err(RenameError::Failed(IoError::other(error.to_string()))),
        }
    }
    Err(RenameError::Failed(IoError::other(
        "the replace could not rename past a sharing lock after the full backoff",
    )))
}

/// Move the temp over the target by path, the fallback a filesystem without the
/// POSIX flag takes. The replace flag lets it overwrite an existing target; a
/// sharing violation retries through the backoff schedule.
#[expect(
    unsafe_code,
    reason = "moves the temp over the target through the kernel API"
)]
fn move_file(temp_wide: &[u16], target_wide: &[u16]) -> IoResult<()> {
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    let sharing = HRESULT::from_win32(ERROR_SHARING_VIOLATION.0);
    for step in backoff_schedule() {
        match unsafe {
            MoveFileExW(
                PCWSTR(temp_wide.as_ptr()),
                PCWSTR(target_wide.as_ptr()),
                flags,
            )
        } {
            Ok(()) => return Ok(()),
            Err(error) if error.code() == sharing => {
                sleep(step);
                continue;
            }
            Err(error) => return Err(IoError::other(error.to_string())),
        }
    }
    Err(IoError::other(
        "the replace could not move past a sharing lock after the full backoff",
    ))
}

/// Delete the temp file left behind by a failed replace.
#[expect(unsafe_code, reason = "deletes the temp through the kernel API")]
fn delete_file(temp_wide: &[u16]) -> IoResult<()> {
    unsafe { DeleteFileW(PCWSTR(temp_wide.as_ptr())) }.map_err(|e| IoError::other(e.to_string()))
}

/// The outcome of a POSIX-semantics rename attempt: done, unsupported by the
/// filesystem, or failed for any other reason.
enum RenameError {
    /// The filesystem rejected the POSIX flag; the caller falls back to a move.
    Unsupported,
    /// The rename failed for a reason the fallback cannot help with.
    Failed(IoError),
}

/// Build the byte buffer the rename call takes: the replace and POSIX flags, a
/// null root directory, the wide name length in bytes, and the wide name. Built
/// from bytes rather than a struct so the variable-length name and the
/// union-field layout are explicit and the buffer is one allocation.
fn build_rename_info(target_wide: &[u16]) -> Vec<u8> {
    const REPLACE_IF_EXISTS: u32 = 0x1;
    const POSIX_SEMANTICS: u32 = 0x2;
    const HEADER: usize = 20;
    let name_bytes = target_wide.len() * 2;
    let mut buf = vec![0u8; HEADER + name_bytes];
    buf[0..4].copy_from_slice(&(REPLACE_IF_EXISTS | POSIX_SEMANTICS).to_le_bytes());
    buf[16..20].copy_from_slice(&(name_bytes as u32).to_le_bytes());
    for (index, unit) in target_wide.iter().enumerate() {
        buf[HEADER + index * 2..][..2].copy_from_slice(&unit.to_le_bytes());
    }
    buf
}

/// A null-terminated wide string, the form the file API takes.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(once(0)).collect()
}

/// The message for a lexical rejection, so the caller sees which guard refused
/// the path rather than a generic error.
fn lexical_message(reason: LexicalRejection) -> &'static str {
    match reason {
        LexicalRejection::UncShare => "the path names a network share",
        LexicalRejection::ReservedDevice => "the path names a reserved device",
        LexicalRejection::AlternateDataStream => "the path names an alternate data stream",
    }
}
