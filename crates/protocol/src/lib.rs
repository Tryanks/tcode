//! Serializable contract between tcode clients and hosts.
//!
//! Data-carrying enums deliberately use explicit `type`/`content` tagging.
//! Unknown data-carrying variants are decode errors; callers should use the
//! wire helpers, which turn those errors into [`ProtocolError`] values.

mod command;
mod event;
mod preview;
mod pull_request;
mod query;
pub use preview::{PreviewRequest, PreviewResponse};
pub use pull_request::{
    MAX_PULL_REQUEST_MEDIA_BYTES, PullRequestAction, PullRequestActionResult,
    PullRequestActionState, PullRequestActor, PullRequestCapabilities, PullRequestComment,
    PullRequestConversation, PullRequestFile, PullRequestFileText, PullRequestFiles,
    PullRequestLabel, PullRequestLabelCandidate, PullRequestLabelCandidates, PullRequestMedia,
    PullRequestMergeState, PullRequestPatch, PullRequestPermissions, PullRequestReaction,
    PullRequestReactionContent, PullRequestRead, PullRequestReadResponse, PullRequestRejection,
    PullRequestReviewAnchor, PullRequestReviewState, PullRequestReviewThread,
    PullRequestReviewVerdict, PullRequestReviewer, PullRequestReviewerCandidate,
    PullRequestReviewerCandidates, PullRequestReviewerKind, PullRequestReviewerState,
    PullRequestStackActionState, PullRequestStackHead, PullRequestStackLayerState,
    PullRequestStackPushAccess, PullRequestThreadReplies, PullRequestViewedFiles,
    PullRequestViewedState,
};
pub mod terminal;
mod wire;

pub use command::{Command, CommandResponse, SettingsPatch, TerminalSelection, ThreadExportFormat};
pub use event::{
    AcpMarketplaceItem, AgentStatus, ArchivedSessions, EventEnvelope, ExternalImportState,
    ExternalImportStatus, ForkAvailability, GitActionRequest, GitStatusStatus, IndexSnapshot,
    IndexSummary, InjectedChannel, InjectedInstructions, InjectedPullRequestTools,
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
    STORED_OUTPUT_ROWS, SessionSearchHit, SourceTool, SpaceAction, SpaceInfo, TraverseLookupState,
    TraverseLookupStatus, TraverseManifestState, TraverseManifestStatus, TraverseRelayStatus,
    TraverseSourceStatus,
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
// ReadItemImage for fetching model-read image bytes outside timeline windows;
// version 11 (v0.2.3) replaces SessionActivity.background_only with waiting, which
// also covers unfinished child threads and a finished child awaiting settle,
// carries a dispatched child's execution, delivery and latest run in
// SessionActivity.agent and adds SessionActivity.failed; adds the
// CreateNewProject, StartScratchDraft, SetProjectRoot (with the
// WorkingDirectoryMissing runtime error), CancelAgent, PinSession,
// UnpinSession, ReorderPinned and ReorderActive commands and thread metadata's
// pinned_at, pin_order, active_order and cancelled_at; gives thread metadata
// Settled/Active overrides and lifecycle timestamps, with UnsettleSession
// replacing MakeSessionActive, SetAutoSettle and global and per-project
// settlement settings, and removes idle AutoArchiveSweep/ArchivedCount, the
// idle-auto-archive settings, Settings.collapsed_threads, SetThreadCollapsed
// and orchestrate archive_on_complete; stored messages carry a
// human/agent/server origin; the Preview topic no longer names a session;
// Settings.traverse and SettingsPatch::Traverse carry a list of sources,
// official or self-hosted, and HostingState.traverse reports each source's
// manifest, relays with latency and home relay, and pkarr lookup checks; adds
// pull requests: linked PRs in SessionMeta with LinkPullRequest,
// UnlinkPullRequest and the PullRequestLinked result, WatchPullRequest with
// the PullRequestWatch toast and SessionStatus.pull_request_tools,
// Query::PullRequest reading files (paged by an opaque cursor in
// PullRequestFiles.next_cursor), file text, conversation, review thread
// replies, viewed files, media, label and reviewer candidates (by opaque id),
// ActionState and StackState, SetPullRequestFilesViewed, RefreshPullRequest,
// RunPullRequestAction for comments, reviews, replies and resolutions,
// reactions (typed names), edits, labels, reviewers, ready/draft, close,
// reopen, revert, branch update, merge, auto-merge, MergeStack and RebaseStack
// with their typed results (UpToDate, Queued, AutoMergeEnabled, Opened,
// Pending, MergeUnconfirmed, RebaseStarted, Rebased, RebaseStopped) and the
// StackUnknown and Unsupported rejections, EditPullRequestReviewDraft with
// pull_request_reviews and pull_request_operations on thread metadata, the
// PullRequestStack toast, settings project_merge_methods and
// remove_agent_credits_on_merge with their patches, snapshots whose
// additions, deletions and changed_files may be null, and host capabilities
// (update_merge, host_viewed_marks, merge_message, labels) on the
// conversation and action state; Settings.github becomes
// Settings.source_control, whose hosts carry a kind (github, forgejo, gitea,
// gitlab, bitbucket, azure) and a per-host status with kind, added, credential
// source (saved_basic with its email, saved_bearer), resolution order, problem
// (which may be unverified), token_url, access_token_url and token_scopes,
// patched by SourceControlHost and RemoveSourceControlHost, with SetHostToken
// (optional email) and RefreshHostCredentials replacing SetGitHubToken and
// RefreshGitHubCredentials.
//
// Version 12 (v0.2.4) gives thread metadata branch_pull_request, the pull
// request discovery found on the thread's checkout branch, shown without a
// link; the pull request source created becomes opened, and links stored as
// created, which branch discovery made, are dropped as metadata is read;
// pull request mergeability adds pending, a host still working it out, beside
// unknown; the PullRequestWatch toast's notice gains merged; and
// SessionStatus.pull_request_tools carries attached and replaces the
// instructions string with a list of verbatim texts, each with the channel
// that carries it (the linking instructions through the provider's
// instruction channel, an MCP server's instructions, Codex's developer
// instructions).
pub const PROTOCOL_VERSION: u32 = 12;

#[cfg(test)]
mod tests;
