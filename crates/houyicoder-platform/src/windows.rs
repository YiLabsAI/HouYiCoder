//! Windows system proxy reader, credential reader, shell discovery, the atomic
//! file replace, and the console boundary a terminal session runs against.

mod fs;
mod net;
mod secrets;
pub(crate) mod shell;
pub(crate) mod terminal;

pub(crate) use fs::atomic_replace;
pub(crate) use net::read_system_proxy;
pub(crate) use secrets::read_secret;
