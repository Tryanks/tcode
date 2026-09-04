//! Remote transport, pairing, discovery, and multi-client multiplexing.

pub mod auth;
pub mod client;
pub mod discovery;
pub mod mux;
pub mod server;
mod wire;

pub use mux::{Connection, HostMux};
pub use server::{PairingCode, RemoteConfig, RemoteServer, StaticBundle, serve};
