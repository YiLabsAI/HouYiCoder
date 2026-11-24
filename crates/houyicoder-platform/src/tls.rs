//! TLS provider selection: the one place a crypto backend is installed for
//! the whole process.
//!
//! reqwest is built with the no-provider TLS feature so its aws-lc backend
//! never enters the build (its C toolchain does not cross-compile to Windows
//! from this host). That leaves the process without a TLS provider until one
//! is installed here. The ring provider is the workspace's chosen backend:
//! pure Rust, already a dependency for other crypto, and its install is
//! idempotent, so every caller can run it before building a client.

use rustls::crypto::CryptoProvider;
use rustls::crypto::ring::default_provider;

/// Install the ring crypto provider as the process-wide TLS backend.
///
/// Idempotent and safe to call more than once: a second install returns a
/// false that callers ignore, and the first install wins. Returns false when
/// another provider was already installed, which is not an error, because the
/// provider in place is the one that will be used.
pub fn install_tls_provider() -> bool {
    CryptoProvider::install_default(default_provider()).is_ok()
}
