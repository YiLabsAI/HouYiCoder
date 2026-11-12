//! Windows system proxy reader, shell discovery, and the console boundary a
//! terminal session runs against.

mod net;
pub(crate) mod shell;
pub(crate) mod terminal;

pub(crate) use net::read_system_proxy;
