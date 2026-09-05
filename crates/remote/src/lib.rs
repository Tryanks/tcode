//! Remote transport, pairing, discovery, and multi-client multiplexing.

#[cfg(feature = "server")]
mod auth;
#[cfg(feature = "client")]
pub mod client;
pub mod discovery;
#[cfg(feature = "server")]
pub mod mux;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
mod wire;

#[cfg(feature = "server")]
pub use mux::{Connection, HostMux};
#[cfg(feature = "server")]
pub use server::{DeviceInfo, PairingCode, RemoteConfig, RemoteServer, StaticBundle, serve};
