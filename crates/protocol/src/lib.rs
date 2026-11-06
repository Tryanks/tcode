//! Serializable contract between tcode clients and hosts.
//!
//! Data-carrying enums deliberately use explicit `type`/`content` tagging.
//! Unknown data-carrying variants are decode errors; callers should use the
//! wire helpers, which turn those errors into [`ProtocolError`] values.

mod command;
mod event;
mod index;
pub use index::index_fingerprint;
mod original;
pub use original::{OriginalMetadata, OriginalReply, OriginalRequest, OriginalTarget};
mod preview;
mod pull_request;
mod query;
pub use preview::{
    DevServer, PreviewClient, PreviewRequest, PreviewResponse, PreviewSession, PreviewTab,
};
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

pub use command::{
    ChildSettlement, Command, CommandResponse, SettingsPatch, SettleChildrenResult,
    TerminalSelection, ThreadExportFormat,
};
pub use event::{
    AccountDiskUsage, AcpMarketplaceItem, AgentStatus, ArchivedSessions, ControlActivityEntry,
    ControlStatus, DiskUsage, DiskUsagePart, DiskUsageState, EndpointDescription,
    EndpointPresetDescription, EventEnvelope, ExternalImportState, ExternalImportStatus,
    ForkAvailability, GitActionRequest, GitStatusStatus, HarnessDescription, IndexSnapshot,
    IndexSummary, InjectedChannel, InjectedInstructions, InjectedServer, InjectedTools,
    MergeWorktreeFailure, NoticeSeverity, PendingRewind, PluginCatalogState, PluginChallenge,
    PluginChallengeKind, PluginOperationTarget, PluginStaleReason, ProviderDiskUsage,
    ProviderPluginCatalog, ProviderUpdateAvailable, ProviderUpdateRun, ProviderVersionStatus,
    ProvidersStatus, QueuedMessageStatus, RewindBlocked, RewindStatus, RuntimeEffect, RuntimeError,
    RuntimeNotice, RuntimeNotification, RuntimeOperationId, RuntimeToast, Scope,
    ScopedProviderChoice, ServerEvent, SessionActivity, SessionEventRecord, SessionPlan,
    SessionStatus, StorageMigration, TcodeDiskUsage, TcodeUpdateStatus, TerminalContextStatus,
    TerminalSplitStatus, TerminalStatus, ThreadReferenceStatus, Topic,
};
pub use query::{
    DeviceAccess, ExternalThread, GitDiffResult, GitDiffScope, GitFileText, GroupAction, GroupInfo,
    GroupMachineInfo, HostedDevice, HostingAction, HostingState, IconImageEntry,
    MAX_SESSION_HISTORY_BYTES, MAX_THREAD_EXPORT_BYTES, OUTPUT_PREVIEW_BYTES, PathEntry, PathInfo,
    PathKind, Query, QueryResponse, RecentDir, SESSION_HISTORY_RECORDS, SESSION_WINDOW_BYTES,
    STORED_OUTPUT_BYTES, STORED_OUTPUT_COLS, STORED_OUTPUT_HISTORY, STORED_OUTPUT_ROWS,
    SessionSearchHit, SourceTool, SpaceAction, SpaceInfo, TraverseLookupState,
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
//
// Version 13 (v0.2.5) gives SessionMeta pull_request_watches_paused for
// retained watch subscriptions; ReadFileBytes accepts an optional max_bytes
// read budget, capped by the host attachment policy; ReadItemImagePreview
// returns optional previews of recorded tool images; ReadImagePreview returns
// optional size-specific static attachment previews; ReadItemImage accepts
// the same optional network read budget; TurnRetry events,
// SessionStatus.turn_retry and the RetryTurn command support native capacity
// continuations; the agent event stream gains steer_rejected, a provider's
// report that the correlated steer reached it after its turn had ended, after
// which the host sends the text as an ordinary turn; index subscriptions may
// carry known_index, with matching content receiving IndexUnchanged instead
// of a snapshot; ReadFilePreview rejects files larger than 512 KiB before
// transferring bytes; SessionUsageChanged carries usage and meter capacity
// when the rest of the session status is unchanged, while subscription replies
// remain full statuses; IndexActivitiesChanged sends changed activity entries
// when other index facts are unchanged, while subscription replies remain full
// index snapshots; explicit original reads use a raw-byte iroh bi-stream or
// the authenticated browser /original endpoint, with bounded
// ReadFileBytes/ReadItemImage control queries unchanged; Project.quick_chat
// identifies host-owned quick chats in replicated metadata; and the protocol
// adds SettleChildren and its ChildrenSettled command response.
//
// Version 14 (v0.2.6) adds the CommandOutput topic and its
// CommandOutputReplaced event, pushed while a command's output grows, which
// replace the RenderStoredOutput query and the TerminalFrame query response.
//
// Version 15 (v0.2.7): AgentStatus gains `report`, what a dispatched child
// owes its lead (none, expected, reported, unreported, failed), and
// SessionMeta carries the child's `expectation`; the protocol adds the
// CompactContext command, which asks a provider with a native compaction
// operation to compact now.
//
// Version 16 (v0.3.0-rc.1) adds machine groups: HostingAction gains Group
// with GroupAction (create, join, leave, regenerate and pause the group link,
// remove a machine, admit a declined device); HostingState carries the group
// with its link, machines and declined devices; HostedDevice marks group
// members; and RevokeDevice removes a group member from every machine of the
// group.
//
// Version 17 (v0.3.0-rc.2): a machine in a group pairs devices and machines
// with its one-time invitation alone, so GroupAction loses RegenerateLink and
// SetLinkEnabled, Create and Join lose their device lists (every full-access
// device is brought), GroupInfo loses its link fields and HostedDevice its
// group mark.
//
// Version 18 (v0.3.0-rc.3): ReadImagePreview's crop_to_square becomes cover,
// and image previews keep the source's shape instead of being cropped to the
// tile.
//
// Version 19 (v0.3.0): an original's metadata, the OriginalReady reply and
// the browser /original response (X-Image-Dimensions) carry the image's
// displayed size; the provider_relay event and timeline entry carry `prompt`,
// the handoff as sent to the new provider; the agent event stream gains
// goal_updated, SessionStatus carries the harness's standing goal, and the
// protocol adds the SetGoal and ClearGoal commands. tcode_control (#929):
// SessionMeta carries control_grant, control_grant_requests, references,
// follow_up, parked_at, settle_suggestion_dismissed_at, self_settle,
// started_by and control_watches; Settings carries control and
// settlement_automation with the ControlEnabled, ControlDefaultGrant,
// ControlProjectGrant and SettlementAutomation patches; the agent event stream
// gains control_action; the protocol adds the SetControlGrant,
// RespondControlGrant, AddThreadReference, RemoveThreadReference, SetFollowUp,
// SetParked, DismissSettleSuggestion and RevokeSelfSettle commands and the
// ControlActivity and ReviewRecords queries; SessionStatus.pull_request_tools
// (InjectedPullRequestTools) becomes injected_tools (InjectedTools), listing
// every offered server's tools with the servers that offer them
// (InjectedServer), and SessionStatus gains control; RuntimeNotice gains
// review_recorded and reference_boundary_crossed; AddThreadReference answers
// ThreadReferenceAdded. Thread moves: the protocol adds the MoveSession
// command, SessionMeta carries moved_from, and the provider_relay event and
// timeline entry carry moved_from, the directory a moved thread's handoff
// left.
//
// Version 20 (v0.3.1): the protocol adds the SetPullRequestWatchesPaused
// command, and Interrupt no longer pauses a thread's watches between turns.
//
// Version 21 (v0.3.2): SessionMeta drops branch_pull_request; a thread shows
// only the pull requests linked to it. ApprovalRequest carries parent_item_id,
// the subagent that asked, and SessionStatus carries pending approvals and
// input while the provider is live, not only during a turn.
//
// Version 22 (v0.3.3): the protocol adds the MoveQueued command and the
// ThreadAnchorTurn query.
//
// Version 23 (v0.3.4): the assistant_message item carries `summarized`, set
// when the provider delivered a summary of the message rather than its text.
//
// Version 24 (v0.4.0-rc.1): provider sign-in commands (StartProviderLogin,
// SubmitProviderLoginCode, CancelProviderLogin, LogoutProvider) with a
// provider snapshot's `login`, the ProfileId response to
// CreateAccountProfile, CreateThirdPartyProfile's `inherit_config`, the
// ProfileConfig runtime error and the SharedCredentials probe diagnostic;
// the plan item kind with plan approvals, decision and timeline feedback,
// and no per_prompt_system instruction channel; a command output frame's
// scrolled-off history and lines_evicted; one snapshot answering an events
// subscription to the end of the log; canonical tool_call items from every
// provider (output, image_reads, Codex sleep and image_generation, Grok's
// server/tool names) stored with item_version 1; a session plan's
// explanation; and the item_backgrounded event.
//
// Version 25 (v0.4.0-rc.2): SessionMeta drops orchestrate_enabled and
// result_max_chars and carries session_transfer_from; subagents start
// through tcode_control; SessionActivity's latest_activity; favorite_models
// rows {provider, profile_id, model} and ToggleFavoriteModel's provider and
// profile_id; TerminalDelta's scrolled and the CommandOutputDelta push; the
// Hosting topic with HostingReplaced instead of polling Query::Hosting;
// TerminalInput and SetTerminalHeight without a delivery key; the
// ProviderUsageChanged event; Settings' scroll_to_latest_on_send with the
// ScrollToLatestOnSend patch; and the provider_batch_line_break runtime
// error.
//
// Unreleased: the host holds each thread's preview (PreviewSession, its
// renderer, tabs and user_acted_since_snapshot) on the new PreviewSession
// topic with PreviewSessionReplaced; the Preview topic carries the
// subscribing client (PreviewClient) and only the requests of the threads it
// renders, with PreviewRenderingReplaced naming those threads; the protocol
// adds the ShowPreviewHere, ReportPreviewTabs and ReportPreviewInput
// commands; and a status request's reply carries userActedSinceSnapshot and
// renderedOn. PreviewRequest's Click and Type take an optional selector and
// an optional ref, and Scroll an optional ref; a snapshot's reply is the
// page agent's outline (url, title, loading, generation, elements, outline,
// omitted) instead of interactiveElements and visibleText; a status reply
// adds generation and console. PreviewRequest::Screenshot is a
// struct variant with an optional ref and full_page. The DevServers topic
// carries the host's dev servers for one thread (DevServersReplaced with
// DevServer port, pid, process and cwd). The settings_unreadable runtime
// error. The StorageMigration topic carries the host's move of an older
// build's threads into its storage (StorageMigrationReplaced with
// StorageMigration moved, total and waiting_for_space, or null when none
// runs). The DiskUsage topic carries the host's last disk-usage measurement
// (DiskUsageReplaced with DiskUsageState last and measuring; DiskUsage
// measured_at, tcode, providers; TcodeDiskUsage dir, conversations, images,
// worktrees, other; ProviderDiskUsage provider, bytes, accounts;
// AccountDiskUsage profile_id, dirs, bytes, parts; DiskUsagePart name,
// bytes), and the MeasureDiskUsage command measures again. Settings drops
// auto_open_task_panel with the AutoOpenTaskPanel patch. The host no longer
// stops a turn on a model change: ServerEvent drops ModelFallbackBlocked and
// FallbackReviewReady; Settings drops abort_on_model_fallback,
// fallback_review_advisor and fallback_review with the AbortOnModelFallback,
// FallbackReviewAdvisor, FallbackReviewProvider, FallbackReviewModel and
// FallbackReviewProfileId patches; and the model_fallback_detected and
// turn_blocked agent events are no longer emitted, only read from stored
// logs. Orchestrate goes: the protocol drops the OrchestrateTurn command and
// the OrchestrateDecisionModels and OrchestrateChildModels patches, and
// Settings' orchestrate carries only child_approval and child_worktrees.
// Code platforms: a source-control host's status carries login, the account
// its token signs in as, and its problem may be rejected (status, at); the
// protocol adds the VerifyHostToken command, which saves a token only when
// the host takes it, and its TokenCheck response (accepted with an optional
// account, rejected, missing_scopes, unreachable, unconfirmed).
// Computer use asks before typing with an app in front: Settings'
// computer_use drops allow_foreground_fallback for foreground_default and
// foreground_apps (a stored true reads as allow), the
// ComputerUseForegroundDefault and ComputerUseForegroundApp patches replace
// ComputerUseAllowForegroundFallback, SessionStatus carries
// foreground_prompt, and the protocol adds RespondForegroundPrompt.
// Plugin management has no switches: Settings drops plugins with the
// PluginManagementEnabled and PluginManagementProvider patches, and the host
// keeps two catalogs per profile, one listed from outside any project and one
// for the project last asked about, so ProvidersStatus.plugins may name a
// profile twice, told apart by context_cwd; ProviderPluginEntry adds
// latest_version and always_on, and PluginInstallation adds other_project.
// Harness settings: ProvidersStatus carries `harnesses`, one HarnessDescription per
// native harness and installed ACP agent; ProfileSettingsPatch is adjacently
// tagged with one variant per field (SetHarnessEnabled, SetDisplayName,
// SetAccentColor, SetEnv, SetSharedEnv, SetBinaryPath, SetHomePath,
// SetLaunchArgs, SetToggle, SetCustomModels, SetHiddenModels) instead of
// ReplaceConfiguration; ProviderSettings' shared_env; CreateThirdPartyProfile
// names its provider; AddCustomAcpAgent drops env; an installed ACP agent's
// env is EnvVar rows and AcpAgentPatch has SetLaunchArgs and SetEnv instead
// of SetLaunchOptions; ProviderLogin's AwaitingDevice and the MissingApiKey
// probe diagnostic.
// Rewind follows each harness's own: RewindTurn takes an optional steer,
// checkpoint_id and client_id (PreviewRewind a steer and checkpoint_id); the
// PreviewRewind command (answered with
// CommandResponse::RewindPreview) and the UndoRewind command are new;
// ServerEvent::NativeRewindPrefill becomes RewindPrefill (client_id,
// attachments); SessionStatus drops native_rewind_blocked,
// native_rewind_pending and native_rewind_prefill_available for rewind
// (RewindStatus with RewindBlocked and PendingRewind) and queue_held;
// RuntimeError drops native_rewind_blocked and RuntimeNotice
// native_rewind_completed. The rewind agent events change (RewindFailed's
// failure, RewindCompleted's later_history, files, incomplete, undo_token
// and prefill_attachments) and SteerCheckpoint, CheckpointsCleared,
// RewindAvailability, RewindPreviewed, RewindUndone and RewindUndoFailed
// are new.
pub const PROTOCOL_VERSION: u32 = 25;

#[cfg(test)]
mod tests;
