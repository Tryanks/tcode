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
    PluginCatalogState, PluginChallenge, PluginChallengeKind, PluginOperationTarget,
    PluginStaleReason, ProviderPluginCatalog, ProviderUpdateAvailable, ProviderUpdateRun,
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
    ClientMessage, ClientPayload, HostMessage, MAX_LINE_BYTES, ProtocolError, Subscription,
    decode_client_line, decode_host_line, encode_line,
};

// The number changes once per release whose wire differs from the previous
// release's, when the release is cut (CONTRIBUTING.md, principle 9). A wire
// change between releases keeps the number and adds a note under "unreleased";
// cutting the release bumps the number once and folds the notes into its line.
//
// Version 4 adds client-generated command deduplication keys; version 5 moves
// authentication into the transport, so hello carries no token; version 6
// sends index and history changes instead of whole replacements and
// compresses the native transport; version 7 carries the running turn and
// the requests it waits on in the session status; version 8 adds the Cursor
// and Grok provider kinds, which an older peer cannot decode because
// `ProviderKind` has no unknown fallback; version 9 sends Orchestrate rows that
// leave bundled guidance out, which an older peer would read as no guidance.
//
// Unreleased: provider plugin catalogs, their commands, challenges and switches;
// provider update availability merged per check round, UpdateProvider replaced
// by UpdateProviders, and sequential update run status with toast progress;
// MarkSessionRead, the client's acknowledgement that a thread's conversation
// loaded, replaces marking a thread read when it is subscribed.
pub const PROTOCOL_VERSION: u32 = 9;

#[cfg(test)]
mod tests;
