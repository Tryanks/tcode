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
    AcpMarketplaceItem, ArchivedSessions, EventEnvelope, ExternalImportState, ExternalImportStatus,
    ForkAvailability, GitActionRequest, GitStatusStatus, IndexSnapshot, IndexSummary,
    MergeWorktreeFailure, NoticeSeverity, PluginCatalogState, PluginChallenge, PluginChallengeKind,
    PluginOperationTarget, PluginStaleReason, ProviderPluginCatalog, ProviderUpdateAvailable,
    ProviderUpdateRun, ProviderVersionStatus, ProvidersStatus, QueuedMessageStatus, RuntimeEffect,
    RuntimeError, RuntimeNotice, RuntimeNotification, RuntimeOperationId, RuntimeToast, Scope,
    ScopedProviderChoice, ServerEvent, SessionActivity, SessionEventRecord, SessionPlan,
    SessionStatus, TcodeUpdateStatus, TerminalContextStatus, TerminalSplitStatus, TerminalStatus,
    Topic,
};
pub use query::{
    DeviceAccess, ExternalThread, GitDiffResult, GitDiffScope, GitFileText, HostedDevice,
    HostingAction, HostingState, IconImageEntry, MAX_SESSION_HISTORY_BYTES,
    MAX_THREAD_EXPORT_BYTES, OUTPUT_PREVIEW_BYTES, PathEntry, PathInfo, PathKind, Query,
    QueryResponse, RecentDir, SESSION_HISTORY_RECORDS, SESSION_WINDOW_BYTES, STORED_OUTPUT_COLS,
    STORED_OUTPUT_ROWS, SessionSearchHit, SourceTool, SpaceAction, SpaceInfo,
};
pub use terminal::{TerminalDelta, TerminalFrame};
pub use wire::{
    ClientMessage, ClientPayload, HostMessage, MAX_LINE_BYTES, Principal, ProtocolError,
    Subscription, decode_client_line, decode_host_line, encode_line,
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
// the requests it waits on in the session status; version 8 (v0.2.0) adds
// the Cursor and Grok provider kinds, which an older peer cannot decode
// because `ProviderKind` has no unknown fallback, sends Orchestrate rows that
// leave bundled guidance out, adds provider plugin catalogs with their
// commands, challenges and switches, merges provider update availability per
// check round with UpdateProviders replacing UpdateProvider and a sequential
// update run status, replaces marking a thread read when subscribed with the
// client's MarkSessionRead, moves working/turn_running into host-authored
// session activity with action availability, full usage and meter capacity, a
// separate session plan topic, shared worktree facts and archived-list
// revisions, makes session log cursors stored row positions, and acknowledges
// a subscription to an unloaded thread after its window; version 9 (v0.2.1)
// removes the app Plan commands, interaction mode, proposed-plan snapshots,
// SelectUltrathink and ultrathink_armed, so an older peer sending a removed
// command is rejected, replaces approval modes with native permission options
// and requested selections, makes Orchestrate child approval Auto the default,
// and adds Shared Spaces: transport principals with a policy revision, the
// scope and space-index topics with correlated scope snapshots, space hosting
// actions including atomic member removal with link rotation and the created
// space id, device access, optional stored-event authors, and a space id on
// pairing with the space name in the paired reply and two pairing rejections;
// version 10 (v0.2.2) adds ImageRead, ToolCall.image_reads metadata and
// ReadItemImage for fetching model-read image bytes outside timeline windows.
//
// Unreleased: replaces SessionActivity.background_only with waiting, which
// also covers unfinished child threads.
pub const PROTOCOL_VERSION: u32 = 10;

#[cfg(test)]
mod tests;
