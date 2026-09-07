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
    GitStatusStatus, IndexSnapshot, MergeWorktreeFailure, NoticeSeverity, ProviderVersionStatus,
    ProvidersStatus, QueuedMessageStatus, RuntimeEffect, RuntimeError, RuntimeNotice,
    RuntimeNotification, RuntimeOperationId, RuntimeToast, ServerEvent, SessionEventRecord,
    SessionStatus, TcodeUpdateStatus, TerminalContextStatus, TerminalSplitStatus, TerminalStatus,
    Topic,
};
pub use query::{
    ExternalThread, GitDiffResult, GitDiffScope, GitFileText, MAX_SESSION_HISTORY_BYTES,
    MAX_THREAD_EXPORT_BYTES, PathEntry, Query, QueryResponse, RecentDir, SESSION_HISTORY_RECORDS,
    STORED_OUTPUT_COLS, STORED_OUTPUT_ROWS, SessionSearchHit, SourceTool,
};
pub use terminal::{TerminalDelta, TerminalFrame};
pub use wire::{
    ClientMessage, ClientPayload, HostMessage, ProtocolError, Subscription, decode_client_line,
    decode_host_line, encode_line,
};

// Older clients interpret a nonzero fresh snapshot offset as a corrupt tail.
pub const PROTOCOL_VERSION: u32 = 3;

#[cfg(test)]
mod tests;
