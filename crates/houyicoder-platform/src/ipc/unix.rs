//! The Unix domain socket a detached session listens on: binding it under the
//! owner-only permission a shared host requires, and the endpoint naming, the
//! client connection, and the removal a caller needs to run and reap one.

use std::fs::remove_file;
use std::io::{Error, ErrorKind, Result};
use std::os::unix::net::UnixStream;
use std::path::Path;

use tokio::net::UnixListener;

use super::{LocalConnection, LocalStreams, set_owner_only};

/// A bound session endpoint, waiting for clients.
pub struct LocalListener {
    endpoint: String,
    inner: UnixListener,
}

impl LocalListener {
    /// Bind the socket at the endpoint path and restrict it to its owner.
    pub fn bind(endpoint: &str) -> Result<Self> {
        let path = Path::new(endpoint);
        let inner = UnixListener::bind(path)?;
        set_owner_only(path)?;
        Ok(Self {
            endpoint: endpoint.to_string(),
            inner,
        })
    }

    /// The endpoint this listener is bound to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Wait for the next client.
    pub async fn accept(&mut self) -> Result<LocalConnection> {
        let (stream, _) = self.inner.accept().await?;
        Ok(Box::new(stream))
    }
}

/// Name and bind a session's endpoint in one step, clearing the socket file a
/// predecessor left behind.
pub fn bind_session(sessions_dir: &Path, session_id: &str) -> Result<LocalListener> {
    let endpoint = session_endpoint(sessions_dir, session_id)?;
    remove_endpoint(&endpoint)?;
    LocalListener::bind(&endpoint)
}

/// The endpoint a session listens on: a socket file inside the session
/// directory.
pub fn session_endpoint(sessions_dir: &Path, session_id: &str) -> Result<String> {
    let path = sessions_dir.join(format!("{session_id}.sock"));
    path.into_os_string().into_string().map_err(|_| {
        Error::new(
            ErrorKind::InvalidInput,
            "the session endpoint path is not valid text",
        )
    })
}

/// Connect a client to a session endpoint, as the two directions a carrier
/// that reads on its own thread owns separately.
pub fn connect_local_streams(endpoint: &str) -> Result<LocalStreams> {
    let stream = UnixStream::connect(endpoint)?;
    let read = stream.try_clone()?;
    Ok(LocalStreams {
        read: Box::new(read),
        write: Box::new(stream),
    })
}

/// Remove the socket file an endpoint names, treating an absent file as
/// already removed.
pub fn remove_endpoint(endpoint: &str) -> Result<()> {
    match remove_file(endpoint) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
