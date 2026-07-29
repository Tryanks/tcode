use std::path::PathBuf;

use agent::{AgentEvent, ProviderKind};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use tcode_core::{
    acp::AcpAgentPatch,
    project::Project,
    session::{ReviewComment, ReviewSide},
    settings::{ProfileSettingsPatch, Settings},
    ui::{RightTab, TerminalSplitDirection, WorkspaceMode},
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

#[test]
fn round_trips_top_level_wire_types() {
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        app_version: "0.1.0".into(),
        capabilities: vec!["terminal".into()],
    };
    round_trip(&hello);
    round_trip(&HelloAck {
        protocol_version: PROTOCOL_VERSION,
        app_version: "0.1.0".into(),
        capabilities: vec!["terminal".into()],
    });

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
        after_seq: Some(4),
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
        seq: 9,
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
        seq: 10,
        event: ServerEvent::SessionStatusReplaced(SessionStatus {
            session_id: "session-1".into(),
            title: "Replicated status".into(),
            cwd: PathBuf::from("/tmp/project"),
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
            }],
            delivery_in_flight: Some(4),
            turn_running: true,
            working: true,
            pending_approval: false,
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
    round_trip(&ProtocolError {
        code: "not_found".into(),
        message: "missing".into(),
    });
    round_trip(&ReverseRequest {
        method: "preview.click".into(),
        params: json!({"selector": "#submit"}),
    });
    round_trip(&ReverseResponse {
        request_id: 44,
        result: Ok(json!({"clicked": true})),
    });
}

#[test]
fn round_trips_commands_for_serialized_ui_mutation_gaps() {
    let commands = [
        Command::UpdateProfileSettings {
            profile_id: "codex".into(),
            patch: ProfileSettingsPatch::SetEnabled { enabled: false },
        },
        Command::UpdateAcpAgent {
            id: "gemini".into(),
            patch: AcpAgentPatch::SetLaunchOptions {
                env: vec![("KEY".into(), "value".into())],
                launch_args: Some("--flag".into()),
            },
        },
        Command::SplitTerminal {
            direction: TerminalSplitDirection::Vertical,
        },
        Command::AddReviewComment {
            comment: ReviewComment::new(
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
            ),
        },
        Command::SelectDiffTurn { turn: 3 },
        Command::SetDraftWorkspace {
            mode: WorkspaceMode::NewWorktree {
                base: "main".into(),
            },
        },
        Command::SetRightTab {
            tab: RightTab::Plan,
        },
        Command::WriteRelaunchMarker {
            reopen_settings: "computer_use".into(),
        },
    ];
    for command in commands {
        round_trip(&command);
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
    round_trip(&RuntimeSnapshot {
        notifications: vec![RuntimeNotification::Effect(
            RuntimeEffect::CopyToClipboard {
                text: "plan".into(),
            },
        )],
    });
    round_trip(&ServerEvent::TerminalOutput {
        bytes: vec![0, b'\n', 255],
    });
    round_trip(&TerminalSnapshot {
        bytes: vec![1, 2, 3],
        exit_code: Some(0),
    });
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
        payload: ClientPayload::Query(Query::SecretPresence {
            profile_id: "claude".into(),
            name: "ANTHROPIC_API_KEY".into(),
        }),
    };
    let line = encode_line(&message).unwrap();
    assert!(line.ends_with('\n'));
    let decoded = decode_client_line(&line).unwrap();
    match decoded.payload {
        ClientPayload::Query(Query::SecretPresence { profile_id, name }) => {
            assert_eq!(decoded.id, 42);
            assert_eq!(profile_id, "claude");
            assert_eq!(name, "ANTHROPIC_API_KEY");
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
