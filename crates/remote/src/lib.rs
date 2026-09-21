//! The browser listener (static bundle, password login, `/ws`), the native
//! client host on Traverse, and the preview bridge.

#[cfg(feature = "server")]
mod auth;
#[cfg(feature = "client")]
pub mod client_host;
#[cfg(feature = "server")]
pub mod server;
#[cfg(any(feature = "server", feature = "client"))]
mod wire;

#[cfg(feature = "client")]
pub use client_host::NativeClientHost;
#[cfg(feature = "server")]
pub use server::{HostingHandler, RemoteConfig, RemoteServer, StaticBundle, serve};
pub use tcode_traverse::{Connection, HostMux};

#[cfg(feature = "client")]
pub mod preview;
