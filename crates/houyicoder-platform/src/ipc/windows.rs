//! The named pipe a detached session listens on: creating it with an
//! owner-only access list, first-instance exclusivity, and remote clients
//! refused, plus the endpoint naming, the client connection, and the removal a
//! caller needs to run and reap one.

use std::io::{Error, Read, Result, Write};
use std::iter::once;
use std::mem::replace;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tracing::warn;
use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, HANDLE, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::Win32::System::Threading::{CreateEventW, ResetEvent};
use windows::core::PCWSTR;

use super::{LocalConnection, LocalStreams, current_user_scope, new_nonce, session_pipe_name};
use crate::windows::pipe::{
    PipeDirection, SecurityDescriptor, create_instance, owner_only_descriptor,
};

/// How long a client waits for an instance to become free before giving up.
/// The listener creates one as soon as a client is taken, so this bounds a
/// pathologically slow listener rather than a normal attach.
const BUSY_WAIT_MS: u32 = 5_000;

/// A bound session endpoint, waiting for clients.
pub struct LocalListener {
    endpoint: String,
    descriptor: SecurityDescriptor,
    next: NamedPipeServer,
}

impl LocalListener {
    /// Create the pipe with first-instance exclusivity, so a name already
    /// held by another process fails here rather than silently sharing it.
    pub fn bind(endpoint: &str) -> Result<Self> {
        let descriptor = owner_only_descriptor()?;
        let next = create_instance(endpoint, &descriptor, true, PipeDirection::Duplex)?;
        Ok(Self {
            endpoint: endpoint.to_string(),
            descriptor,
            next,
        })
    }

    /// The endpoint this listener is bound to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Wait for the next client. A replacement instance is created before the
    /// connected one is awaited, so a client arriving during the handshake
    /// finds an instance to connect to rather than a busy name. A client that
    /// disconnects before the handshake finishes leaves its instance unusable,
    /// so the loop takes the next client instead of failing the listener.
    pub async fn accept(&mut self) -> Result<LocalConnection> {
        loop {
            let next = create_instance(
                &self.endpoint,
                &self.descriptor,
                false,
                PipeDirection::Duplex,
            )?;
            let connected = replace(&mut self.next, next);
            match connected.connect().await {
                Ok(()) => return Ok(Box::new(Connection { inner: connected })),
                Err(e) => warn!(error = %e, "local client left before the handshake"),
            }
        }
    }
}

/// Name and bind a session's endpoint in one step. A pipe name carries a
/// fresh nonce, so no file is left to clear.
pub fn bind_session(sessions_dir: &Path, session_id: &str) -> Result<LocalListener> {
    let endpoint = session_endpoint(sessions_dir, session_id)?;
    LocalListener::bind(&endpoint)
}

/// A connected pipe instance.
///
/// Tokio's write shutdown only flushes a named pipe, so a split write half
/// that shuts down leaves the client blocked: the server still holds the pipe.
/// Disconnecting is what the peer observes as end of stream, so this wrapper
/// turns shutdown into that operation.
struct Connection {
    inner: NamedPipeServer,
}

impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Ready(Ok(())) => (),
            other => return other,
        }
        match self.inner.disconnect() {
            Ok(()) => Poll::Ready(Ok(())),
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

/// The endpoint a session listens on: a pipe name carrying the account scope
/// and a fresh nonce, so only a process that read the recorded endpoint can
/// name it. A pipe name is global to the machine, so the session directory
/// plays no part.
pub fn session_endpoint(_sessions_dir: &Path, session_id: &str) -> Result<String> {
    let scope = current_user_scope()?;
    let nonce = new_nonce()?;
    Ok(session_pipe_name(&scope, session_id, &nonce))
}

/// Connect a client to a session endpoint, as the two directions a carrier
/// that reads on its own thread owns separately. Each direction waits on its
/// own completion event, so a read waiting for the host never holds a write
/// back.
pub fn connect_local_streams(endpoint: &str) -> Result<LocalStreams> {
    let pipe = open_pipe(endpoint)?;
    let read = pipe.try_clone()?;
    Ok(LocalStreams {
        read: Box::new(OverlappedPipe::new(read)?),
        write: Box::new(OverlappedPipe::new(pipe)?),
    })
}

/// One direction of the client end, driven from its own thread.
///
/// A handle opened without the overlapped flag serializes every read and
/// write on its file object, so a client waiting for the host's next message
/// holds the lock its next keystroke needs: writing while a read is pending
/// waits until a message arrives, which is never if the host is waiting for
/// that keystroke. Each direction here owns a completion event instead, so
/// the two proceed at the same time.
struct OverlappedPipe {
    handle: OwnedHandle,
    event: OwnedHandle,
}

impl OverlappedPipe {
    #[expect(unsafe_code, reason = "creates the completion event for a direction")]
    fn new(handle: OwnedHandle) -> Result<Self> {
        let event =
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(Error::other)?;
        let event = unsafe { OwnedHandle::from_raw_handle(event.0.cast()) };
        Ok(Self { handle, event })
    }

    /// Clear this direction's event and hand the system the record it
    /// completes the operation into. The event is manual-reset, so clearing
    /// it is what attaches the wait below to this operation rather than one
    /// that finished earlier.
    #[expect(unsafe_code, reason = "clears the completion event")]
    fn begin(&self) -> Result<OVERLAPPED> {
        unsafe { ResetEvent(as_handle(self.event.as_raw_handle())) }.map_err(Error::other)?;
        Ok(OVERLAPPED {
            hEvent: as_handle(self.event.as_raw_handle()),
            ..OVERLAPPED::default()
        })
    }

    /// Wait for the operation and answer its byte count. A peer that closed
    /// its end is the end of a read; a write reports it as a failure.
    #[expect(unsafe_code, reason = "waits on the kernel transfer")]
    fn finish(&self, overlapped: &OVERLAPPED, end_on_close: bool) -> Result<usize> {
        let mut transferred = 0u32;
        let waited = unsafe {
            GetOverlappedResult(
                as_handle(self.handle.as_raw_handle()),
                overlapped,
                &mut transferred,
                true,
            )
        };
        match waited {
            Ok(()) => Ok(transferred as usize),
            Err(error)
                if end_on_close
                    && matches!(
                        WIN32_ERROR::from_error(&error),
                        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)
                    ) =>
            {
                Ok(0)
            }
            Err(error) => Err(Error::other(error)),
        }
    }
}

impl Read for OverlappedPipe {
    #[expect(unsafe_code, reason = "issues the kernel read")]
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
        // One call carries what the byte count field expresses; a larger
        // buffer takes another call.
        let length = buffer.len().min(u32::MAX as usize);
        let mut overlapped = self.begin()?;
        let issued = unsafe {
            ReadFile(
                as_handle(self.handle.as_raw_handle()),
                Some(&mut buffer[..length]),
                None,
                Some(&mut overlapped),
            )
        };
        if let Err(error) = issued
            && WIN32_ERROR::from_error(&error) != Some(ERROR_IO_PENDING)
        {
            return Err(Error::other(error));
        }
        self.finish(&overlapped, true)
    }
}

impl Write for OverlappedPipe {
    #[expect(unsafe_code, reason = "issues the kernel write")]
    fn write(&mut self, buffer: &[u8]) -> Result<usize> {
        let length = buffer.len().min(u32::MAX as usize);
        let mut overlapped = self.begin()?;
        let issued = unsafe {
            WriteFile(
                as_handle(self.handle.as_raw_handle()),
                Some(&buffer[..length]),
                None,
                Some(&mut overlapped),
            )
        };
        if let Err(error) = issued
            && WIN32_ERROR::from_error(&error) != Some(ERROR_IO_PENDING)
        {
            return Err(Error::other(error));
        }
        self.finish(&overlapped, false)
    }

    fn flush(&mut self) -> Result<()> {
        // A pipe holds no buffer of its own to empty.
        Ok(())
    }
}

fn as_handle(raw: RawHandle) -> HANDLE {
    HANDLE(raw)
}

/// A pipe disappears when its last handle closes, so there is nothing on disk
/// to remove.
pub fn remove_endpoint(_endpoint: &str) -> Result<()> {
    Ok(())
}

/// Open the client end of the named pipe, waiting while every instance is
/// serving another client. The listener creates a replacement instance as
/// soon as one is taken, so the wait ends once that instance exists. The
/// handle is opened for overlapped I/O, which is what lets a read and a write
/// run at the same time.
#[expect(unsafe_code, reason = "opens the pipe by name")]
fn open_pipe(endpoint: &str) -> Result<OwnedHandle> {
    let name: Vec<u16> = endpoint.encode_utf16().chain(once(0)).collect();
    let name = PCWSTR(name.as_ptr());
    loop {
        let opened = unsafe {
            CreateFileW(
                name,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                None,
            )
        };
        match opened {
            Ok(handle) => return Ok(unsafe { OwnedHandle::from_raw_handle(handle.0.cast()) }),
            Err(e) if WIN32_ERROR::from_error(&e) == Some(ERROR_PIPE_BUSY) => {
                if !unsafe { WaitNamedPipeW(name, BUSY_WAIT_MS) }.as_bool() {
                    return Err(Error::last_os_error());
                }
            }
            Err(e) => return Err(Error::other(e.to_string())),
        }
    }
}
