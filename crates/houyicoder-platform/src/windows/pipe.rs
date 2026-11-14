//! Named pipe instances that carry bytes between this process and a local
//! peer, whether the peer is a client attaching to a session or a command
//! writing its output.
//!
//! An instance this crate creates carries an access list naming its owner
//! alone and refuses remote clients, so no other account on the machine
//! connects to it. A first instance is exclusive: a name another process
//! already holds fails here instead of silently sharing a channel. The
//! instance runs in overlapped mode, which is what lets the runtime read it
//! through the completion port rather than a parked thread.

use std::ffi::c_void;
use std::io::{Error, Result};
use std::iter::once;
use std::mem::size_of;
use std::process::id;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::Win32::Foundation::{FALSE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::core::PCWSTR;

use crate::ipc::{current_user_scope, hex_digits, new_nonce, owner_only_sddl};

/// A security descriptor parsed from its string form, freed as it drops. Each
/// instance copies the access list as it is created, so the descriptor is
/// needed only while instances are created under it.
///
/// The pointer is held as an address rather than as the raw pointer type the
/// interface takes, because a raw pointer would make a holder of this
/// unusable across threads. The descriptor is heap memory allocated here and
/// never shared with another process, so moving it between threads is sound.
pub(crate) struct SecurityDescriptor(usize);

impl SecurityDescriptor {
    #[expect(unsafe_code, reason = "parses a descriptor from its text form")]
    pub(crate) fn parse(sddl: &str) -> Result<Self> {
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

/// The endpoint name one command output stream is created under. The process
/// id and a sequence keep concurrent commands apart, and a fresh nonce keeps
/// another account from predicting the next name and holding it with a first
/// instance of its own.
pub(crate) fn stream_endpoint(label: &str, sequence: u32) -> Result<String> {
    let nonce = new_nonce()?;
    Ok(format!(
        r"\\.\pipe\houyicoder-stdio-{}-{sequence}-{}-{label}",
        id(),
        hex_digits(&nonce)
    ))
}

/// The access list a pipe created here grants: general access to the current
/// account alone.
pub(crate) fn owner_only_descriptor() -> Result<SecurityDescriptor> {
    let scope = current_user_scope()?;
    SecurityDescriptor::parse(&owner_only_sddl(&scope))
}

/// Which directions a pipe instance carries.
pub(crate) enum PipeDirection {
    /// A session channel a client both reads and writes.
    Duplex,
    /// A command output stream only this process reads.
    Inbound,
}

/// Create one pipe instance under the given name and access list. A duplex
/// instance carries a channel a client reads and writes; an inbound instance
/// carries a stream this process only reads.
pub(crate) fn create_instance(
    name: &str,
    descriptor: &SecurityDescriptor,
    first: bool,
    direction: PipeDirection,
) -> Result<NamedPipeServer> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.pointer(),
        bInheritHandle: FALSE,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .access_outbound(matches!(direction, PipeDirection::Duplex));
    if matches!(direction, PipeDirection::Inbound) {
        // One instance means no second instance of the name can appear beside
        // the one the command's write end connected to.
        options.max_instances(1);
    }
    #[expect(unsafe_code, reason = "passes the parsed access list to the creation")]
    unsafe {
        options.create_with_security_attributes_raw(
            name,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    }
}
