//! Windows system proxy reader and shell discovery.

mod net;
pub(crate) mod shell;

pub(crate) use net::read_system_proxy;
