//! Windows system proxy reader, credential reader, the clipboard utility, the
//! atomic file replace, the console boundary a terminal session runs against,
//! the account and random values a session pipe is named and secured with,
//! the native launcher that puts a command inside its job at creation, and
//! the shell discovery.

pub(crate) mod clipboard;
mod fs;
mod net;
pub(crate) mod pipe;
pub(crate) mod process;
mod secrets;
pub(crate) mod shell;
pub(crate) mod terminal;
pub(crate) mod text;
pub(crate) mod user;

pub(crate) use fs::atomic_replace;
pub(crate) use net::read_system_proxy;
pub(crate) use secrets::read_secret;
