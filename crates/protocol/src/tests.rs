use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use agent::{AgentEvent, ProviderKind};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use tcode_core::{
    acp::AcpAgentPatch,
    project::Project,
    session::{ReviewComment, ReviewSide},
    settings::{ProfileSettingsPatch, Settings},
    ui::{TerminalSplitDirection, WorkspaceMode},
};

use super::*;

fn round_trip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).unwrap();
    let decoded = serde_json::from_str::<T>(&json).unwrap();
    assert_eq!(&decoded, value);
}

fn round_trip_client_payload(id: u64, payload: ClientPayload) {
    let message = ClientMessage { id, payload };
    let line = encode_line(&message).expect("encode client NDJSON");
    assert!(line.ends_with('\n'));
    assert_eq!(
        decode_client_line(&line).expect("decode client NDJSON"),
        message
    );
}

#[test]
fn round_trips_top_level_wire_types() {
    let command = Command::SendTurn {
        text: "hello".into(),
        attachment_paths: vec![PathBuf::from("/tmp/image.png")],
    };
    round_trip(&command);
    let query = Query::LoadGitDiff {
        cwd: PathBuf::from("/tmp/project"),
        scope: GitDiffScope::WorkingTree,
        base: None,
        ignore_whitespace: true,
    };
    round_trip(&query);
    let response = QueryResponse::FileBytes(vec![0, 1, 2, 254, 255]);
    round_trip(&response);

    let subscription = Subscription {
        topic: Topic::SessionEvents {
            session_id: "session-1".into(),
        },
    };
    round_trip(&subscription);

    let client = ClientMessage {
        id: 7,
        payload: ClientPayload::Command(command),
    };
    round_trip(&client);
    round_trip(&ClientPayload::Subscribe(subscription));

    let event = EventEnvelope {
        topic: Topic::RuntimeEvents,
        event: ServerEvent::Runtime(RuntimeNotification::Notice(
            RuntimeNotice::UpdateAvailable {
                provider: ProviderKind::Codex,
                version: "1.2.3".into(),
            },
        )),
    };
    round_trip(&event);
    round_trip(&HostMessage::Event(event));
    round_trip(&EventEnvelope {
        topic: Topic::SessionStatus {
            session_id: "session-1".into(),
        },
        event: ServerEvent::SessionStatusReplaced(SessionStatus {
            session_id: "session-1".into(),
            title: "Replicated status".into(),
            cwd: PathBuf::from("/tmp/project"),
            attachments_dir: PathBuf::from("/tmp/data/attachments/session-1"),
            provider: ProviderKind::Codex,
            requested_model: Some("gpt-5".into()),
            requested_profile_id: Some("work".into()),
            acp_agent_id: None,
            project_id: Some("project-1".into()),
            approval_mode: agent::ApprovalMode::Supervised,
            interaction_mode: agent::InteractionMode::Plan,
            queued_messages: vec![QueuedMessageStatus {
                id: 4,
                text: "next".into(),
                fire_at_unix_secs: None,
            }],
            review_comment_drafts: vec![ReviewComment::new(
                "src/lib.rs".into(),
                2,
                2,
                ReviewSide::New,
                "Please revise this.".into(),
                "+new".into(),
                "turn:3".into(),
                "Turn 4".into(),
                1,
                2,
            )],
            terminals: vec![TerminalStatus { id: 7 }],
            active_terminal_id: Some(7),
            terminal_splits: vec![TerminalSplitStatus {
                first: 7,
                second: 8,
                direction: TerminalSplitDirection::Horizontal,
            }],
            terminal_contexts: vec![TerminalContextStatus {
                id: 1,
                terminal_label: "shell".into(),
                line_start: 2,
                line_end: 3,
                text: "selected".into(),
            }],
            terminal_open: true,
            terminal_height: 260.0,
            delivery_in_flight: Some(4),
            turn_running: true,
            working: true,
            pending_approval: false,
            pending_user_input: true,
            supports_steering: true,
            provider_option_descriptors: Vec::new(),
            provider_option_selections: vec![agent::OptionSelection {
                id: "reasoningEffort".into(),
                value: json!("high"),
            }],
            provider_commands: vec![agent::ProviderCommand {
                name: "review".into(),
                description: Some("Review changes".into()),
                kind: agent::ProviderCommandKind::Command,
            }],
            git_branch: Some("main".into()),
            branches: vec!["main".into(), "feature".into()],
            draft: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            worktree: None,
            preparing_worktree: false,
            relay_confirmation: Some(("Claude".into(), "Codex".into())),
            native_rewind_pending: true,
            native_rewind_prefill_available: true,
            model_pending_restart: true,
            options_pending_restart: false,
            approval_pending_restart: false,
            ultrathink_armed: true,
        }),
    });
    round_trip(&EventEnvelope {
        topic: Topic::Providers,
        event: ServerEvent::ProvidersReplaced(ProvidersStatus {
            models_loading: HashMap::from([(ProviderKind::Codex, true)]),
            provider_versions: HashMap::from([(
                ProviderKind::Codex,
                ProviderVersionStatus {
                    installed: Some("1.2.3".into()),
                    latest: Some("1.2.4".into()),
                    update_available: true,
                    checking: false,
                    updating: false,
                    update_command: Some("npm install -g @openai/codex@latest".into()),
                },
            )]),
            provider_snapshots: HashMap::from([(
                "codex".into(),
                tcode_core::provider_status::ProviderSnapshot {
                    installed: true,
                    checking: false,
                    ..Default::default()
                },
            )]),
            acp_marketplace_items: vec![AcpMarketplaceItem {
                id: "agent".into(),
                name: "Agent".into(),
                version: "2.0.0".into(),
                description: "ACP agent".into(),
                installed: false,
                installing: true,
                supported: true,
            }],
            acp_registry_loading: false,
            acp_registry_error: None,
            acp_installing: HashSet::from(["agent".into()]),
            providers_checking: true,
            secret_names: HashMap::from([(
                "codex".into(),
                HashSet::from(["OPENAI_API_KEY".into()]),
            )]),
            ..Default::default()
        }),
    });
    round_trip(&EventEnvelope {
        topic: Topic::GitStatus,
        event: ServerEvent::GitStatusReplaced(GitStatusStatus {
            status: Some(tcode_core::git::GitStatus {
                is_repo: true,
                branch: Some("main".into()),
                changed_files: vec![tcode_core::git::GitFileEntry {
                    path: "src/lib.rs".into(),
                    insertions: 3,
                    deletions: 1,
                }],
                ..Default::default()
            }),
            busy: true,
        }),
    });
    round_trip(&ProtocolError {
        code: "not_found".into(),
        message: "missing".into(),
    });
}

fn assert_command_crosses_ndjson(id: u64, command: Command) {
    match &command {
        Command::ApplyPendingRelaunch => {}
        Command::OpenLatestSession => {}
        Command::ShutdownAllAndFlush => {}
        Command::OrchestrateTurn { .. } => {}
        Command::ReloadProvider => {}
        Command::SetProfileSecret { .. } => {}
        Command::UpdateProfileSettings { .. } => {}
        Command::CreateThirdPartyProfile { .. } => {}
        Command::DeleteProfile { .. } => {}
        Command::RefreshProviderStatus => {}
        Command::CheckProviderVersions => {}
        Command::UpdateProvider { .. } => {}
        Command::SetSidebarCollapsed { .. } => {}
        Command::RunGitAction { .. } => {}
        Command::RefreshAcpRegistry => {}
        Command::InstallAcpAgent { .. } => {}
        Command::RemoveAcpAgent { .. } => {}
        Command::AddCustomAcpAgent { .. } => {}
        Command::UpdateAcpAgent { .. } => {}
        Command::SetActiveAcpAgent { .. } => {}
        Command::ResetSettings => {}
        Command::WriteRelaunchMarker { .. } => {}
        Command::SetTerminalHeight { .. } => {}
        Command::ToggleTerminalPanel => {}
        Command::CloseTerminalPanel => {}
        Command::RestartTerminal => {}
        Command::NewTerminal => {}
        Command::SplitTerminal { .. } => {}
        Command::ActivateTerminal { .. } => {}
        Command::CloseTerminal { .. } => {}
        Command::CaptureTerminalSelection { .. } => {}
        Command::RemoveTerminalContext { .. } => {}
        Command::AddReviewComment { .. } => {}
        Command::RemoveReviewComment { .. } => {}
        Command::CycleProjectSort => {}
        Command::CreateProject { .. } => {}
        Command::StartExternalImport { .. } => {}
        Command::FinishExternalImport { .. } => {}
        Command::ToggleProjectCollapsed { .. } => {}
        Command::UpdateSettings { .. } => {}
        Command::ArchiveSession { .. } => {}
        Command::UnarchiveSession { .. } => {}
        Command::AutoArchiveSweep { .. } => {}
        Command::RenameSession { .. } => {}
        Command::ForkThread { .. } => {}
        Command::DeleteSession { .. } => {}
        Command::DeleteProject { .. } => {}
        Command::MarkSessionUnread { .. } => {}
        Command::StartDraft { .. } => {}
        Command::SetDraftWorkspace { .. } => {}
        Command::SelectSession { .. } => {}
        Command::SendTurn { .. } => {}
        Command::ScheduleTurn { .. } => {}
        Command::ConfirmRelayAndSend { .. } => {}
        Command::Steer { .. } => {}
        Command::SteerQueued { .. } => {}
        Command::DropQueued { .. } => {}
        Command::Interrupt => {}
        Command::RespondApproval { .. } => {}
        Command::RespondUserInput { .. } => {}
        Command::SetActiveModel { .. } => {}
        Command::SetActiveOption { .. } => {}
        Command::SelectUltrathink => {}
        Command::SetInteractionMode { .. } => {}
        Command::ToggleInteractionMode => {}
        Command::ImplementPlan => {}
        Command::DismissPlan => {}
        Command::ImplementPlanInNewThread { .. } => {}
        Command::CopyPlan { .. } => {}
        Command::SavePlanToWorkspace { .. } => {}
        Command::DownloadPlan { .. } => {}
        Command::LoadBranches => {}
        Command::CheckoutBranch { .. } => {}
        Command::SetActiveApprovalMode { .. } => {}
        Command::ToggleFavoriteModel { .. } => {}
        Command::RewindTurn { .. } => {}
    }
    round_trip_client_payload(id, ClientPayload::Command(command));
}

fn assert_query_crosses_ndjson(id: u64, query: Query) {
    match &query {
        Query::ListActiveWorkspace => {}
        Query::ScanExternalHistory => {}
        Query::GenerateCommitMessage { .. } => {}
        Query::LoadGitDiff { .. } => {}
        Query::ReadFileBytes { .. } => {}
        Query::SaveAttachment { .. } => {}
        Query::RemoveUserFile { .. } => {}
        Query::IsDirectory { .. } => {}
    }
    round_trip_client_payload(id, ClientPayload::Query(query));
}

#[test]
fn every_command_and_query_crosses_ndjson() {
    let review = ReviewComment::new(
        "src/lib.rs".into(),
        2,
        4,
        ReviewSide::New,
        "Please simplify this.".into(),
        "+new code".into(),
        "working-tree".into(),
        "Working tree".into(),
        10,
        12,
    );
    let commands = vec![
        Command::ApplyPendingRelaunch,
        Command::OpenLatestSession,
        Command::ShutdownAllAndFlush,
        Command::OrchestrateTurn {
            text: "coordinate".into(),
            attachment_paths: vec![PathBuf::from("/tmp/input.png")],
        },
        Command::ReloadProvider,
        Command::SetProfileSecret {
            profile_id: "claude".into(),
            name: "ANTHROPIC_API_KEY".into(),
            value: Some("secret".into()),
        },
        Command::UpdateProfileSettings {
            profile_id: "codex".into(),
            patch: ProfileSettingsPatch::SetEnabled { enabled: false },
        },
        Command::CreateThirdPartyProfile {
            name: "Local".into(),
            base_url: "http://127.0.0.1:8080".into(),
            model: Some("model".into()),
            api_key: "key".into(),
        },
        Command::DeleteProfile {
            profile_id: "profile".into(),
        },
        Command::RefreshProviderStatus,
        Command::CheckProviderVersions,
        Command::UpdateProvider {
            provider: ProviderKind::Codex,
        },
        Command::SetSidebarCollapsed { collapsed: true },
        Command::RunGitAction {
            action: tcode_core::git::GitAction::Commit,
            message: Some("message".into()),
            included: Some(vec!["src/lib.rs".into()]),
            feature_branch: Some("feature".into()),
        },
        Command::RefreshAcpRegistry,
        Command::InstallAcpAgent { id: "agent".into() },
        Command::RemoveAcpAgent { id: "agent".into() },
        Command::AddCustomAcpAgent {
            name: "agent".into(),
            command: "agent-bin".into(),
            args: vec!["--stdio".into()],
            env: vec![("KEY".into(), "value".into())],
        },
        Command::UpdateAcpAgent {
            id: "agent".into(),
            patch: AcpAgentPatch::SetLaunchOptions {
                env: vec![("KEY".into(), "value".into())],
                launch_args: Some("--flag".into()),
            },
        },
        Command::SetActiveAcpAgent { id: "agent".into() },
        Command::ResetSettings,
        Command::WriteRelaunchMarker {
            reopen_settings: "providers".into(),
        },
        Command::SetTerminalHeight { height: 260.0 },
        Command::ToggleTerminalPanel,
        Command::CloseTerminalPanel,
        Command::RestartTerminal,
        Command::NewTerminal,
        Command::SplitTerminal {
            direction: TerminalSplitDirection::Vertical,
        },
        Command::ActivateTerminal { terminal_id: 7 },
        Command::CloseTerminal { terminal_id: 7 },
        Command::CaptureTerminalSelection { terminal_id: 7 },
        Command::RemoveTerminalContext { context_id: 8 },
        Command::AddReviewComment { comment: review },
        Command::RemoveReviewComment { index: 0 },
        Command::CycleProjectSort,
        Command::CreateProject {
            root: PathBuf::from("/tmp/project"),
        },
        Command::StartExternalImport {
            project_id: "project-1".into(),
            threads: vec![ExternalThread {
                source: SourceTool::CodexCli,
                file: PathBuf::from("/tmp/thread.jsonl"),
                external_id: "external-1".into(),
                title_hint: Some("Imported".into()),
                last_active_ms: 1,
            }],
        },
        Command::FinishExternalImport {
            project_id: "project-1".into(),
        },
        Command::ToggleProjectCollapsed {
            project_id: "project-1".into(),
        },
        Command::UpdateSettings {
            settings: Settings::default(),
        },
        Command::ArchiveSession {
            session_id: "session-1".into(),
        },
        Command::UnarchiveSession {
            session_id: "session-1".into(),
        },
        Command::AutoArchiveSweep {
            project_id: "project-1".into(),
        },
        Command::RenameSession {
            session_id: "session-1".into(),
            title: "Renamed".into(),
        },
        Command::ForkThread {
            id: "session-1".into(),
        },
        Command::DeleteSession {
            session_id: "session-1".into(),
            remove_worktree: true,
        },
        Command::DeleteProject {
            project_id: "project-1".into(),
        },
        Command::MarkSessionUnread {
            session_id: "session-1".into(),
        },
        Command::StartDraft {
            project_id: "project-1".into(),
            cwd: PathBuf::from("/tmp/project"),
        },
        Command::SetDraftWorkspace {
            mode: WorkspaceMode::NewWorktree {
                base: "main".into(),
            },
        },
        Command::SelectSession {
            session_id: "session-1".into(),
        },
        Command::SendTurn {
            text: "hello".into(),
            attachment_paths: vec![PathBuf::from("/tmp/input.png")],
        },
        Command::ScheduleTurn {
            text: "later".into(),
            attachment_paths: Vec::new(),
            fire_at_unix_secs: 123,
        },
        Command::ConfirmRelayAndSend {
            text: "confirmed".into(),
            attachment_paths: Vec::new(),
        },
        Command::Steer {
            text: "adjust".into(),
            attachment_paths: Vec::new(),
        },
        Command::SteerQueued { id: 7 },
        Command::DropQueued { id: 8 },
        Command::Interrupt,
        Command::RespondApproval {
            request_id: "approval-1".into(),
            decision: agent::ApprovalDecision::ApproveForSession,
        },
        Command::RespondUserInput {
            request_id: "input-1".into(),
            answers: serde_json::Map::from_iter([("choice".into(), json!("yes"))]),
        },
        Command::SetActiveOption {
            id: "reasoning_effort".into(),
            value: Some(json!("high")),
        },
        Command::SetActiveModel {
            provider: ProviderKind::Codex,
            model: Some("gpt-5".into()),
            profile_id: Some("codex".into()),
        },
        Command::SelectUltrathink,
        Command::SetInteractionMode {
            mode: agent::InteractionMode::Plan,
        },
        Command::ToggleInteractionMode,
        Command::ImplementPlan,
        Command::DismissPlan,
        Command::ImplementPlanInNewThread {
            title: "Implementation".into(),
        },
        Command::CopyPlan {
            markdown: "# Plan".into(),
        },
        Command::SavePlanToWorkspace {
            markdown: "# Plan".into(),
        },
        Command::DownloadPlan {
            markdown: "# Plan".into(),
            fallback_title: "Plan".into(),
        },
        Command::LoadBranches,
        Command::CheckoutBranch {
            branch: "feature".into(),
        },
        Command::SetActiveApprovalMode {
            mode: agent::ApprovalMode::Supervised,
        },
        Command::ToggleFavoriteModel {
            model: "gpt-5".into(),
        },
        Command::RewindTurn {
            turn: 2,
            mode: agent::RewindMode::FilesAndConversation,
        },
    ];
    for (offset, command) in commands.into_iter().enumerate() {
        assert_command_crosses_ndjson(u64::try_from(offset + 1).unwrap(), command);
    }

    let queries = vec![
        Query::ListActiveWorkspace,
        Query::ScanExternalHistory,
        Query::GenerateCommitMessage {
            included: Some(vec!["src/lib.rs".into()]),
        },
        Query::ReadFileBytes {
            path: PathBuf::from("/tmp/input.bin"),
        },
        Query::LoadGitDiff {
            cwd: PathBuf::from("/tmp/project"),
            scope: GitDiffScope::Branch,
            base: Some("main".into()),
            ignore_whitespace: true,
        },
        Query::SaveAttachment {
            dir: PathBuf::from("/tmp/attachments"),
            bytes: vec![0, b'\n', 255],
            ext: "png".into(),
        },
        Query::RemoveUserFile {
            path: PathBuf::from("/tmp/input.bin"),
        },
        Query::IsDirectory {
            path: PathBuf::from("/tmp/project"),
        },
    ];
    for (offset, query) in queries.into_iter().enumerate() {
        assert_query_crosses_ndjson(u64::try_from(offset + 1_000).unwrap(), query);
    }
}

#[test]
fn every_correlated_response_crosses_ndjson() {
    let command_responses = [
        CommandResponse::Unit,
        CommandResponse::ProjectId(Some("project-1".into())),
        CommandResponse::PendingRelaunchSection(Some("computer_use".into())),
        CommandResponse::ArchivedCount(4),
        CommandResponse::ExternalImportStarted(true),
    ];
    for (offset, response) in command_responses.into_iter().enumerate() {
        let message = HostMessage::Ack {
            id: u64::try_from(offset + 1).unwrap(),
            result: Ok(response),
        };
        let line = encode_line(&message).expect("encode command response");
        assert_eq!(
            decode_host_line(&line).expect("decode command response"),
            message
        );
    }

    let query_responses = vec![
        QueryResponse::ActiveWorkspace(vec![PathEntry {
            rel_path: "src/lib.rs".into(),
            basename: "lib.rs".into(),
            parent: "src".into(),
            is_dir: false,
        }]),
        QueryResponse::ExternalHistory(vec![RecentDir {
            path: PathBuf::from("/tmp/project"),
            last_active_ms: 1,
            threads: Vec::new(),
        }]),
        QueryResponse::CommitMessage("message".into()),
        QueryResponse::GitDiff(GitDiffResult::default()),
        QueryResponse::FileBytes(vec![0, b'\n', 255]),
        QueryResponse::SavedAttachment(PathBuf::from("/tmp/attachment.png")),
        QueryResponse::UserFileRemoved,
        QueryResponse::IsDirectory(true),
    ];
    for (offset, response) in query_responses.into_iter().enumerate() {
        let message = HostMessage::QueryResult {
            id: u64::try_from(offset + 1_000).unwrap(),
            result: Ok(response),
        };
        let line = encode_line(&message).expect("encode query response");
        assert_eq!(
            decode_host_line(&line).expect("decode query response"),
            message
        );
    }
}

#[test]
fn round_trips_event_and_snapshot_families() {
    let stored = SessionEventRecord {
        ts: Some(123),
        event: AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
    };
    assert_eq!(
        serde_json::to_value(&stored).unwrap(),
        json!({
            "ts": 123,
            "event": { "type": "turn_started", "turn_id": "turn-1" }
        })
    );
    round_trip(&stored);
    round_trip(&ServerEvent::SessionEvent(stored.clone()));
    round_trip(&ServerEvent::SessionSnapshot(vec![stored]));

    let project = Project {
        id: "project-1".into(),
        name: "Project".into(),
        root: PathBuf::from("/tmp/project"),
        created_at: 1,
    };
    round_trip(&ServerEvent::IndexUpsertProject(project.clone()));
    round_trip(&IndexSnapshot {
        sessions: Vec::new(),
        projects: vec![project],
    });
    round_trip(&ServerEvent::SettingsSnapshot(Settings::default()));
    round_trip(&ServerEvent::ActiveSessionReplaced(None));
    round_trip(&ServerEvent::NativeRewindPrefill {
        session_id: "session-1".into(),
        text: "restore this prompt".into(),
    });
    round_trip(&RuntimeNotification::Toast(RuntimeToast::GitBusy));
    round_trip(&RuntimeError::ProviderClosed {
        reason: Some("done".into()),
    });
    round_trip(&GitActionRequest {
        action: tcode_core::git::GitAction::Commit,
        message: Some("message".into()),
        included: Some(vec!["src/lib.rs".into()]),
        feature_branch: None,
    });
    round_trip(&RuntimeOperationId(17));
}

#[test]
fn round_trips_query_dto_families() {
    round_trip(&PathEntry {
        rel_path: "src/lib.rs".into(),
        basename: "lib.rs".into(),
        parent: "src".into(),
        is_dir: false,
    });
    round_trip(&RecentDir {
        path: PathBuf::from("/tmp/project"),
        last_active_ms: 12,
        threads: vec![ExternalThread {
            source: SourceTool::CodexCli,
            file: PathBuf::from("/tmp/session.jsonl"),
            external_id: "codex:1".into(),
            title_hint: Some("Title".into()),
            last_active_ms: 11,
        }],
    });
    round_trip(&GitDiffResult {
        texts: vec![GitFileText {
            old: Some("old".into()),
            new: Some("new".into()),
        }],
        ..GitDiffResult::default()
    });
}

#[test]
fn client_message_ndjson_loopback() {
    let message = ClientMessage {
        id: 42,
        payload: ClientPayload::Query(Query::IsDirectory {
            path: PathBuf::from("/tmp/project"),
        }),
    };
    let line = encode_line(&message).unwrap();
    assert!(line.ends_with('\n'));
    let decoded = decode_client_line(&line).unwrap();
    match decoded.payload {
        ClientPayload::Query(Query::IsDirectory { path }) => {
            assert_eq!(decoded.id, 42);
            assert_eq!(path, PathBuf::from("/tmp/project"));
        }
        other => panic!("unexpected payload: {other:?}"),
    }
}

#[test]
fn unknown_command_becomes_protocol_error_without_panicking() {
    let json = r#"{"id":7,"payload":{"type":"command","content":{"type":"future_command","content":{"value":1}}}}"#;
    let result = std::panic::catch_unwind(|| decode_client_line(json));
    let error = result.expect("decoder must not panic").unwrap_err();
    assert_eq!(error.code, "decode_error");
    assert!(error.message.contains("unknown variant"));
}
