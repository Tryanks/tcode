//! Remote transport, pairing, discovery, and multi-client multiplexing.

#[cfg(feature = "server")]
mod auth;
#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "client")]
pub mod client_host;
pub mod discovery;
#[cfg(feature = "server")]
pub mod mux;
#[cfg(feature = "server")]
mod proxy;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
mod wire;

#[cfg(feature = "client")]
pub use client_host::NativeClientHost;
#[cfg(feature = "server")]
pub use mux::{Connection, HostMux};
#[cfg(feature = "server")]
pub use server::{DeviceInfo, PairingCode, RemoteConfig, RemoteServer, StaticBundle, serve};

#[cfg(feature = "client")]
pub mod preview;
