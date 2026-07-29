//! Serializable contract between tcode clients and hosts.
//!
//! Data-carrying enums deliberately use explicit `type`/`content` tagging.
//! Unknown data-carrying variants are decode errors; callers should use the
//! wire helpers, which turn those errors into [`ProtocolError`] values.

mod command;
mod event;
mod query;
mod wire;

pub use command::Command;
pub use event::{
    EventEnvelope, GitActionRequest, IndexSnapshot, RuntimeEffect, RuntimeError, RuntimeNotice,
    RuntimeNotification, RuntimeOperationId, RuntimeSnapshot, RuntimeToast, ServerEvent,
    SessionEventRecord, TerminalSnapshot, Topic,
};
pub use query::{
    ExternalThread, GitDiffResult, GitDiffScope, GitFileText, PathEntry, Query, QueryResponse,
    RecentDir, SourceTool,
};
pub use wire::{
    ClientMessage, ClientPayload, HostMessage, ProtocolError, ReverseRequest, ReverseResponse,
    Subscription, decode_client_line, decode_host_line, encode_line,
};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub app_version: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    pub protocol_version: u32,
    pub app_version: String,
    pub capabilities: Vec<String>,
}

#[cfg(test)]
mod tests;
