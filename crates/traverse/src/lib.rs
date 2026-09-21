//! Traverse: the iroh transport for native Tcode clients.
//!
//! One machine runs a [`TraverseHost`]; devices pair with it once and then
//! open a reconnecting [`Transport`](tcode_client::host::Transport) to it.
//! Everything runs on one process-wide tokio runtime; the public API is
//! synchronous and hands out channels, so the GPUI side never sees tokio.

pub mod client;
pub mod host;
pub mod hosts;
pub mod identity;
pub mod manifest;
pub mod mux;
mod runtime;
mod tunnel;
pub mod wire;

pub use client::{AttachmentTunnels, PairError, connect, pair, pair_blocking};
pub use host::{
    DeviceInfo, EndpointAddrSnapshot, HostConfig, LiveInfo, PairingCode, TraverseHost, TraverseMode,
};
pub use identity::{DeviceIdentity, EndpointOptions};
pub use mux::{Connection, HostMux};
pub use runtime::{block_on, runtime};
