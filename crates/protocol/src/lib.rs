//! Serializable contract between tcode clients and hosts.
//!
//! Data-carrying enums deliberately use explicit `type`/`content` tagging.
//! Unknown data-carrying variants are decode errors; callers should use the
//! wire helpers, which turn those errors into [`ProtocolError`] values.

mod command;
mod event;
mod query;
mod wire;

pub use command::{Command, CommandResponse};
pub use event::{
    AcpMarketplaceItem, EventEnvelope, GitActionRequest, GitStatusStatus, IndexSnapshot,
    ProviderVersionStatus, ProvidersStatus, QueuedMessageStatus, RuntimeEffect, RuntimeError,
    RuntimeNotice, RuntimeNotification, RuntimeOperationId, RuntimeToast, ServerEvent,
    SessionEventRecord, SessionStatus, TerminalContextStatus, TerminalSplitStatus, TerminalStatus,
    Topic,
};
pub use query::{
    ExternalThread, GitDiffResult, GitDiffScope, GitFileText, PathEntry, Query, QueryResponse,
    RecentDir, SourceTool,
};
pub use wire::{
    ClientMessage, ClientPayload, HostMessage, ProtocolError, Subscription, decode_client_line,
    decode_host_line, encode_line,
};

pub const PROTOCOL_VERSION: u32 = 1;

#[cfg(test)]
mod tests;
