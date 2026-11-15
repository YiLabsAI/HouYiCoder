//! Linux system readers: the proxy settings and the clipboard utilities.

pub(crate) mod clipboard;
mod net;

pub(crate) use net::read_system_proxy;
