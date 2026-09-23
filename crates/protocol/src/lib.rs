//! Serializable contract between tcode clients and hosts.
//!
//! Data-carrying enums deliberately use explicit `type`/`content` tagging.
//! Unknown data-carrying variants are decode errors; callers should use the
//! wire helpers, which turn those errors into [`ProtocolError`] values.

mod command;
mod event;
mod preview;
mod query;
pub use preview::{PreviewRequest, PreviewResponse};
pub mod terminal;
mod wire;

pub use command::{Command, CommandResponse, SettingsPatch, TerminalSelection, ThreadExportFormat};
pub use event::{
    AcpMarketplaceItem, EventEnvelope, ExternalImportState, ExternalImportStatus, GitActionRequest,
    GitStatusStatus, IndexSnapshot, IndexSummary, MergeWorktreeFailure, NoticeSeverity,
    ProviderVersionStatus, ProvidersStatus, QueuedMessageStatus, RuntimeEffect, RuntimeError,
    RuntimeNotice, RuntimeNotification, RuntimeOperationId, RuntimeToast, ServerEvent,
    SessionEventRecord, SessionStatus, TcodeUpdateStatus, TerminalContextStatus,
    TerminalSplitStatus, TerminalStatus, Topic,
};
pub use query::{
    ExternalThread, GitDiffResult, GitDiffScope, GitFileText, HostedDevice, HostingAction,
    HostingState, IconImageEntry, MAX_SESSION_HISTORY_BYTES, MAX_THREAD_EXPORT_BYTES,
    OUTPUT_PREVIEW_BYTES, PathEntry, PathInfo, PathKind, Query, QueryResponse, RecentDir,
    SESSION_HISTORY_RECORDS, SESSION_WINDOW_BYTES, STORED_OUTPUT_COLS, STORED_OUTPUT_ROWS,
    SessionSearchHit, SourceTool,
};
pub use terminal::{TerminalDelta, TerminalFrame};
pub use wire::{
    ClientMessage, ClientPayload, HostMessage, ProtocolError, Subscription, decode_client_line,
    decode_host_line, encode_line,
};

// Version 4 adds client-generated command deduplication keys; version 5 moves
// authentication into the transport, so hello carries no token; version 6
// sends index and history changes instead of whole replacements and
// compresses the native transport.
pub const PROTOCOL_VERSION: u32 = 6;

#[cfg(test)]
mod tests;
