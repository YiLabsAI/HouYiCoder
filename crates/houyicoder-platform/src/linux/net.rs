//! Linux has no single system proxy registry. Desktop and container
//! installations set the standard environment variables, which the
//! platform-agnostic environment parser already reads, so the system reader
//! returns direct and the environment decides whether a proxy applies.

use crate::host::net::ProxyResolution;

pub(crate) fn read_system_proxy() -> ProxyResolution {
    ProxyResolution::direct()
}
