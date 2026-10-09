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
    IndexSummary, InjectedPullRequestTools, MergeWorktreeFailure, NoticeSeverity,
    PluginCatalogState, PluginChallenge, PluginChallengeKind, PluginOperationTarget,
    PluginStaleReason, ProviderPluginCatalog, ProviderUpdateAvailable, ProviderUpdateRun,
    ProviderVersionStatus, ProvidersStatus, QueuedMessageStatus, RuntimeEffect, RuntimeError,
    RuntimeNotice, RuntimeNotification, RuntimeOperationId, RuntimeToast, Scope,
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
// ReadItemImage for fetching model-read image bytes outside timeline windows.
//
// Unreleased: replaces SessionActivity.background_only with waiting, which
// also covers unfinished child threads; adds the CreateNewProject and
// StartScratchDraft commands; adds per-host GitHub settings, credential source
// discovery, SetGitHubToken and RefreshGitHubCredentials.
// Unreleased: linked PRs in SessionMeta; LinkPullRequest and UnlinkPullRequest commands,
// and the PullRequestLinked command result.
// Unreleased: Settled/Active overrides and lifecycle timestamps on thread
// metadata; UnsettleSession replaces MakeSessionActive, SetAutoSettle controls
// the durable per-thread disable; global and per-project settlement settings;
// SessionActivity.failed. Idle AutoArchiveSweep/ArchivedCount and the
// idle-auto-archive settings are removed.
// Unreleased: stored messages carry human/agent/server origin; absent origin
// uses the historical top-level human / dispatched child agent fallback.
// Unreleased: adds the SetProjectRoot command and the WorkingDirectoryMissing
// runtime error.
// Unreleased: a pull request watch on linked PRs; the WatchPullRequest command,
// the PullRequestWatch toast and SessionStatus.pull_request_tools.
// Unreleased: SessionActivity.agent carries a dispatched child's execution,
// delivery and latest run start and end, and waiting also covers a finished
// child awaiting settle;
// thread metadata carries cancelled_at; CancelAgent cancels a dispatched
// child from a client; Settings.collapsed_threads,
// SetThreadCollapsed, orchestrate archive_on_complete (settings, metadata and
// OrchestrateArchiveOnComplete) are removed.
// Unreleased: the Preview topic no longer names a session, so a subscriber
// answers automation for threads it is not viewing.
// Unreleased: Query::PullRequest reads a linked pull request's files, file
// text, conversation, review thread replies, viewed files and media;
// SetPullRequestFilesViewed marks or unmarks files as viewed;
// RefreshPullRequest drops the host's reads of one and syncs it.
// Unreleased: thread metadata carries pinned_at, pin_order and active_order;
// adds the PinSession, UnpinSession, ReorderPinned and ReorderActive commands.
// Unreleased: RunPullRequestAction writes to a linked pull request (comment,
// review, thread reply and resolution, reactions, edits, labels, reviewers)
// and answers PullRequestAction with a typed result; EditPullRequestReviewDraft
// edits the host-owned review draft that thread metadata carries as
// pull_request_reviews; Query::PullRequest reads label and reviewer
// candidates; a reaction's content is a typed name; the conversation carries
// the account's permissions, labels and reviewers, and comments and threads
// what the account may do to them.
// Unreleased: RunPullRequestAction also marks ready, converts to draft,
// closes, reopens, reverts, updates the branch, merges and disables
// auto-merge, answering UpToDate, Queued, AutoMergeEnabled and Opened, or a
// rejection InStack or StackUnknown; Query::PullRequest reads ActionState;
// settings carry project_merge_methods and remove_agent_credits_on_merge,
// patched by ProjectMergeMethod and RemoveAgentCreditsOnMerge.
// Unreleased: RunPullRequestAction merges a native stack through GitHub's
// asynchronous merge (MergeStack) and rebases one on the host (RebaseStack),
// answering Pending, MergeUnconfirmed, RebaseStarted, Rebased or RebaseStopped,
// or the stack rejections; InStack is removed; Query::PullRequest reads
// StackState; thread metadata carries pull_request_operations, the stack writes
// running or unconfirmed and a rebase's end until the next full sync; the
// PullRequestStack toast reports how one ended.
// Unreleased: Settings.traverse and SettingsPatch::Traverse carry a list of
// sources, each official or a self-hosted URL with its own switch.
// Unreleased: HostingState.traverse reports each Traverse source's manifest,
// relays with their latency and home relay, and pkarr lookup checks.
// Unreleased: a files read names the page it asks for by an opaque cursor, which
// PullRequestFiles.next_cursor carries in place of next_page; reviewers carry
// an opaque id and labels an id that SetLabels names them by; the conversation
// and the action state carry the host's capabilities.
// Unreleased: Settings.github becomes Settings.source_control, whose hosts carry
// a kind (github, forgejo, gitea) and whose status is per host with its kind,
// origin, credential source, resolution order and problem; SettingsPatch's
// GitHubHost becomes SourceControlHost and RemoveSourceControlHost is added;
// SetGitHubToken and RefreshGitHubCredentials become SetHostToken and
// RefreshHostCredentials; capabilities carry host_viewed_marks.
pub const PROTOCOL_VERSION: u32 = 10;

#[cfg(test)]
mod tests;
