//! The named pipe a detached session listens on: creating it with an
//! owner-only access list, first-instance exclusivity, and remote clients
//! refused, plus the endpoint naming, the client connection, and the removal a
//! caller needs to run and reap one.

use std::ffi::c_void;
use std::fs::File;
use std::io::{Error, Result};
use std::iter::once;
use std::mem::{replace, size_of};
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tracing::warn;
use windows::Win32::Foundation::{
    ERROR_PIPE_BUSY, FALSE, GENERIC_READ, GENERIC_WRITE, HLOCAL, LocalFree, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::core::PCWSTR;

use super::{
    LocalConnection, LocalStreams, current_user_scope, new_nonce, owner_only_sddl,
    session_pipe_name,
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
        let scope = current_user_scope()?;
        let descriptor = SecurityDescriptor::parse(&owner_only_sddl(&scope))?;
        let next = create_instance(endpoint, &descriptor, true)?;
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
            let next = create_instance(&self.endpoint, &self.descriptor, false)?;
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
/// that reads on its own thread owns separately.
pub fn connect_local_streams(endpoint: &str) -> Result<LocalStreams> {
    let pipe = open_pipe(endpoint)?;
    let read = pipe.try_clone()?;
    Ok(LocalStreams {
        read: Box::new(read),
        write: Box::new(pipe),
    })
}

/// A pipe disappears when its last handle closes, so there is nothing on disk
/// to remove.
pub fn remove_endpoint(_endpoint: &str) -> Result<()> {
    Ok(())
}

/// A security descriptor parsed from its string form, freed as it drops. The
/// descriptor outlives every instance created with it, because each instance
/// carries its own copy of the access list.
///
/// The pointer is held as an address rather than as the raw pointer type the
/// interface takes, because a raw pointer would make the listener unusable
/// across threads. The descriptor is heap memory allocated here and never
/// shared with another process, so moving it between threads is sound.
struct SecurityDescriptor(usize);

impl SecurityDescriptor {
    #[expect(unsafe_code, reason = "parses a descriptor from its text form")]
    fn parse(sddl: &str) -> Result<Self> {
        let text: Vec<u16> = sddl.encode_utf16().chain(once(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|e| Error::other(e.to_string()))?;
        Ok(Self(descriptor.0 as usize))
    }

    /// The descriptor as the creation call takes it.
    fn pointer(&self) -> *mut c_void {
        self.0 as *mut c_void
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        #[expect(unsafe_code, reason = "releases the descriptor allocated above")]
        let _ = unsafe { LocalFree(Some(HLOCAL(self.pointer()))) };
    }
}

/// Create one pipe instance under the given name and access list.
#[expect(unsafe_code, reason = "passes the parsed access list to the creation")]
fn create_instance(
    endpoint: &str,
    descriptor: &SecurityDescriptor,
    first: bool,
) -> Result<NamedPipeServer> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.pointer(),
        bInheritHandle: FALSE,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    unsafe {
        options.create_with_security_attributes_raw(
            endpoint,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    }
}

/// Open the client end of the named pipe, waiting while every instance is
/// serving another client. The listener creates a replacement instance as
/// soon as one is taken, so the wait ends once that instance exists.
#[expect(unsafe_code, reason = "opens the pipe by name")]
fn open_pipe(endpoint: &str) -> Result<File> {
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
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };
        match opened {
            Ok(handle) => return Ok(unsafe { File::from_raw_handle(handle.0.cast()) }),
            Err(e) if WIN32_ERROR::from_error(&e) == Some(ERROR_PIPE_BUSY) => {
                if !unsafe { WaitNamedPipeW(name, BUSY_WAIT_MS) }.as_bool() {
                    return Err(Error::last_os_error());
                }
            }
            Err(e) => return Err(Error::other(e.to_string())),
        }
    }
}
