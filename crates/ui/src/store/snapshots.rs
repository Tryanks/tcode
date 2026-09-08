use std::path::PathBuf;

use tcode_core::{
    project::WorktreeInfo,
    session::Timeline,
    settings::Settings,
    ui::{RightTab, WorkspaceMode},
};
use tcode_protocol::{ProvidersStatus, QueuedMessageStatus, SessionStatus, TerminalContextStatus};

use crate::conversation_ui::ConversationUiState;

#[derive(Clone)]
pub(crate) struct ComposerActiveModel {
    pub provider: agent::ProviderKind,
    pub model: Option<String>,
    pub acp_agent_id: Option<String>,
    pub profile_id: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ComposerCheckoutState {
    pub branch: String,
    pub branches: Vec<String>,
    pub turn_running: bool,
    pub is_draft: bool,
    pub worktree_base: Option<String>,
    pub worktree: Option<WorktreeInfo>,
}

#[derive(Clone)]
pub(crate) struct ComposerQueue {
    pub messages: Vec<QueuedMessageStatus>,
    pub can_steer: bool,
    pub agent: &'static str,
}

/// The complete replica-derived state consumed by composer views in one frame.
#[derive(Clone)]
pub(crate) struct ComposerState {
    pub has_active_session: bool,
    pub terminal_contexts: Vec<TerminalContextStatus>,
    pub relay_confirmation: Option<(String, String)>,
    pub active_cwd: Option<PathBuf>,
    pub provider_commands: Vec<agent::ProviderCommand>,
    pub attachments_dir: Option<PathBuf>,
    pub pending_user_input: Option<(String, Vec<agent::UserInputQuestion>)>,
    pub active_model: Option<ComposerActiveModel>,
    pub model_pending_restart: bool,
    pub active_model_spec: Option<agent::ModelSpec>,
    pub active_option_descriptors: Vec<agent::OptionDescriptor>,
    pub active_option_selections: Vec<agent::OptionSelection>,
    pub ultrathink_armed: bool,
    pub options_pending_restart: bool,
    pub interaction_mode: agent::InteractionMode,
    pub token_usage: Option<agent::TokenUsage>,
    /// Account rate-limit windows for the profile driving this session.
    pub usage: Option<tcode_core::usage::ProviderUsage>,
    pub provider: Option<agent::ProviderKind>,
    pub approval_mode: agent::ApprovalMode,
    pub native_approval_modes_enabled: bool,
    pub approval_pending_restart: bool,
    pub queue: Option<ComposerQueue>,
    pub steering_supported: bool,
    pub preparing_worktree: bool,
    pub plan_ready_markdown: Option<String>,
    pub checkout: Option<ComposerCheckoutState>,
    pub turn_running: bool,
    pub pending_approval: Option<agent::ApprovalRequest>,
    pub pending_approval_count: usize,
}

pub(crate) fn composer_state(
    status: Option<&SessionStatus>,
    timeline: Option<&Timeline>,
    settings: &Settings,
    providers: &ProvidersStatus,
) -> ComposerState {
    let provider = status.map(|status| status.provider);
    let native_approval_modes_enabled = status.is_none_or(|status| {
        !status
            .provider
            .caps()
            .downgrade_approval_without_native_approvals
            || status
                .requested_profile_id
                .as_deref()
                .and_then(|id| settings.resolved_profile(id))
                .map(|profile| profile.settings.pi.native_approvals)
                .unwrap_or_else(|| {
                    settings
                        .provider(agent::ProviderKind::Pi)
                        .pi
                        .native_approvals
                })
    });
    let raw_approval_mode = status
        .map(|status| status.approval_mode)
        .unwrap_or_default();
    let approval_mode = if !native_approval_modes_enabled
        && matches!(
            raw_approval_mode,
            agent::ApprovalMode::Supervised | agent::ApprovalMode::AutoAcceptEdits
        ) {
        agent::ApprovalMode::FullAccess
    } else {
        raw_approval_mode
    };
    let active_model_spec = status.and_then(|status| {
        let model = status.requested_model.as_deref()?;
        providers
            .model_catalogs
            .get(&status.provider)?
            .iter()
            .find(|spec| spec.id == model)
            .cloned()
    });
    let checkout = status.and_then(|status| {
        let branch = status.git_branch.clone().or_else(|| {
            status
                .worktree
                .as_ref()
                .map(|worktree| worktree.branch.clone())
        })?;
        Some(ComposerCheckoutState {
            branch,
            branches: status.branches.clone(),
            turn_running: status.turn_running,
            is_draft: status.draft,
            worktree_base: match &status.draft_workspace {
                WorkspaceMode::NewWorktree { base } => Some(base.clone()),
                _ => None,
            },
            worktree: status.worktree.clone(),
        })
    });
    let token_usage = timeline
        .and_then(|timeline| timeline.usage)
        .map(|mut usage| {
            if let Some(status) = status
                && status.provider == agent::ProviderKind::ClaudeCode
                && let (Some(model), Some(reported)) =
                    (status.requested_model.as_deref(), usage.context_window)
            {
                let resolved = agent::claude::resolved_context_window(
                    model,
                    &status.provider_option_selections,
                );
                usage.context_window = Some(reported.min(resolved));
            }
            usage
        });

    // The session names its profile explicitly only when it is not on the
    // provider's built-in one; both resolve into the same usage map.
    let usage = status.and_then(|status| {
        let profile_id = status
            .requested_profile_id
            .clone()
            .unwrap_or_else(|| Settings::builtin_profile_id(status.provider).to_owned());
        settings
            .resolved_profile(&profile_id)
            .filter(|profile| profile.supports_account_usage())?;
        providers.provider_usage.get(&profile_id).cloned()
    });

    ComposerState {
        has_active_session: status.is_some(),
        terminal_contexts: status
            .map(|status| status.terminal_contexts.clone())
            .unwrap_or_default(),
        relay_confirmation: status.and_then(|status| status.relay_confirmation.clone()),
        active_cwd: status.map(|status| status.cwd.clone()),
        provider_commands: status
            .map(|status| status.provider_commands.clone())
            .unwrap_or_default(),
        attachments_dir: status.map(|status| status.attachments_dir.clone()),
        pending_user_input: timeline.and_then(|timeline| timeline.pending_user_input.clone()),
        active_model: status.map(|status| ComposerActiveModel {
            provider: status.provider,
            model: status.requested_model.clone(),
            acp_agent_id: status.acp_agent_id.clone(),
            profile_id: status.requested_profile_id.clone(),
        }),
        model_pending_restart: status.is_some_and(|status| status.model_pending_restart),
        active_model_spec,
        active_option_descriptors: status
            .map(|status| status.provider_option_descriptors.clone())
            .unwrap_or_default(),
        active_option_selections: status
            .map(|status| status.provider_option_selections.clone())
            .unwrap_or_default(),
        ultrathink_armed: status.is_some_and(|status| status.ultrathink_armed),
        options_pending_restart: status.is_some_and(|status| status.options_pending_restart),
        interaction_mode: status
            .map(|status| status.interaction_mode)
            .unwrap_or_default(),
        token_usage,
        usage,
        provider,
        approval_mode,
        native_approval_modes_enabled,
        approval_pending_restart: status.is_some_and(|status| status.approval_pending_restart),
        queue: status.map(|status| ComposerQueue {
            messages: status.queued_messages.clone(),
            can_steer: status.steering_supported,
            agent: status.provider.display_name(),
        }),
        steering_supported: status.is_some_and(|status| status.steering_supported),
        preparing_worktree: status.is_some_and(|status| status.preparing_worktree),
        plan_ready_markdown: timeline
            .and_then(Timeline::plan_ready)
            .map(|plan| plan.markdown.clone()),
        checkout,
        turn_running: status.is_some_and(|status| status.turn_running),
        pending_approval: timeline.and_then(|timeline| timeline.pending_approvals.first().cloned()),
        pending_approval_count: timeline.map_or(0, |timeline| timeline.pending_approvals.len()),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PanelState {
    pub right_panel_open: bool,
    pub right_tab: RightTab,
    pub right_panel_expanded: bool,
    pub terminal_open: bool,
    pub terminal_height: f32,
    pub plan_tab_active: bool,
}

pub(crate) fn panel_state(
    ui: Option<&ConversationUiState>,
    status: Option<&SessionStatus>,
    timeline: Option<&Timeline>,
) -> PanelState {
    PanelState {
        right_panel_open: ui.is_some_and(|ui| ui.right_panel_open),
        right_tab: ui.map_or_else(RightTab::default, |ui| ui.right_tab),
        right_panel_expanded: ui.is_some_and(|ui| ui.right_panel_expanded),
        terminal_open: ui.is_some_and(|ui| ui.terminal_open),
        terminal_height: ui.map_or(240., |ui| ui.terminal_height),
        plan_tab_active: timeline.is_some_and(|timeline| timeline.proposed_plan.is_some())
            || status.is_some_and(|status| status.interaction_mode == agent::InteractionMode::Plan),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_status() -> SessionStatus {
        SessionStatus {
            session_id: "session-1".into(),
            title: "Test".into(),
            cwd: PathBuf::from("/workspace"),
            attachments_dir: PathBuf::from("/attachments"),
            provider: agent::ProviderKind::Codex,
            requested_model: Some("gpt-test".into()),
            requested_profile_id: None,
            acp_agent_id: None,
            project_id: Some("project-1".into()),
            approval_mode: agent::ApprovalMode::Supervised,
            interaction_mode: agent::InteractionMode::Build,
            queued_messages: Vec::new(),
            review_comment_drafts: Vec::new(),
            terminals: Vec::new(),
            active_terminal_id: None,
            terminal_splits: Vec::new(),
            terminal_contexts: Vec::new(),
            terminal_open: false,
            terminal_height: 240.,
            delivery_in_flight: None,
            turn_running: false,
            working: false,
            pending_approval: false,
            pending_user_input: false,
            steering_supported: true,
            provider_option_descriptors: Vec::new(),
            provider_option_selections: Vec::new(),
            provider_commands: Vec::new(),
            git_branch: Some("main".into()),
            branches: vec!["main".into()],
            draft: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            worktree: None,
            preparing_worktree: false,
            relay_confirmation: None,
            native_rewind_pending: false,
            native_rewind_prefill_available: false,
            model_pending_restart: false,
            options_pending_restart: false,
            approval_pending_restart: false,
            ultrathink_armed: false,
        }
    }

    #[test]
    fn composer_state_exposes_first_pending_approval_and_count() {
        let request = agent::ApprovalRequest {
            id: "approval-1".into(),
            turn_id: Some("turn-1".into()),
            kind: agent::ApprovalKind::FileRead {
                detail: "read src/lib.rs".into(),
            },
            options: Vec::new(),
        };
        let mut timeline = Timeline::default();
        timeline.pending_approvals = vec![request.clone(), request.clone()];

        let state = composer_state(
            Some(&session_status()),
            Some(&timeline),
            &Settings::default(),
            &ProvidersStatus::default(),
        );
        assert_eq!(state.pending_approval, Some(request));
        assert_eq!(state.pending_approval_count, 2);
    }

    #[test]
    fn composer_state_clamps_claude_context_window_to_selected_limit() {
        let mut status = session_status();
        status.provider = agent::ProviderKind::ClaudeCode;
        status.requested_model = Some("claude-sonnet-4-6".into());
        status.provider_option_selections = vec![agent::OptionSelection {
            id: "contextWindow".into(),
            value: serde_json::json!(500_000),
        }];
        let mut timeline = Timeline::default();
        timeline.usage = Some(agent::TokenUsage {
            context_window: Some(1_000_000),
            ..Default::default()
        });

        let state = composer_state(
            Some(&status),
            Some(&timeline),
            &Settings::default(),
            &ProvidersStatus::default(),
        );

        assert_eq!(
            state.token_usage.and_then(|usage| usage.context_window),
            Some(500_000)
        );
    }
    #[test]
    fn account_usage_eligibility_does_not_hide_session_context_or_supported_errors() {
        let mut settings: Settings = serde_json::from_str(r#"{"profiles":{"custom":{"kind":"claude_code","env":[{"name":"ANTHROPIC_BASE_URL","value":"https://api.example.com/anthropic"}]}}}"#).unwrap();
        let mut status = session_status();
        status.provider = agent::ProviderKind::ClaudeCode;
        status.requested_profile_id = Some("custom".into());
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &agent::AgentEvent::TokenUsage(agent::TokenUsage {
                freshness: agent::ContextFreshness::Current,
                used_tokens: Some(1234),
                ..Default::default()
            }),
        );
        let mut providers = ProvidersStatus::default();
        providers.provider_usage.insert(
            "custom".into(),
            tcode_core::usage::ProviderUsage {
                error: Some("temporarily unreachable".into()),
                ..Default::default()
            },
        );
        let custom = composer_state(Some(&status), Some(&timeline), &settings, &providers);
        assert_eq!(custom.token_usage.unwrap().used_tokens, Some(1234));
        assert!(custom.usage.is_none());
        settings
            .profiles
            .get_mut("custom")
            .unwrap()
            .settings
            .env
            .clear();
        let native = composer_state(Some(&status), Some(&timeline), &settings, &providers);
        assert_eq!(
            native.usage.unwrap().error.as_deref(),
            Some("temporarily unreachable")
        );
        assert_eq!(native.token_usage.unwrap().used_tokens, Some(1234));
    }

    #[test]
    #[cfg(unix)]
    fn claude_usage_replays_adapter_timeline_and_composer_across_resume() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "tcode-usage-replay-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let binary = root.join("claude-fixture");
        std::fs::write(&binary, "#!/bin/sh\ncase \"$*\" in *--version*) echo '2.1.200'; exit;; esac\nIFS= read -r request\nif [ \"$TCODE_USAGE_INTERRUPTED\" = 1 ]; then IFS= read -r request; fi\ncat \"$TCODE_USAGE_FIXTURE\"\nwhile IFS= read -r request; do :; done\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut timeline = Timeline::default();
        let mut replay = Timeline::default();
        let mut status = session_status();
        status.provider = agent::ProviderKind::ClaudeCode;
        status.requested_model = Some("claude-opus-5".into());
        status.provider_option_selections = vec![agent::OptionSelection {
            id: "contextWindow".into(),
            value: serde_json::json!(300000),
        }];
        let settings = Settings::default();
        let providers = ProvidersStatus::default();
        let mut recorded_events = Vec::new();
        for (index, fixture) in [
            include_str!("../../../agent/tests/fixtures/claude/usage_scope.jsonl"),
            include_str!("../../../agent/tests/fixtures/claude/usage_resume.jsonl"),
            include_str!("../../../agent/tests/fixtures/claude/usage_recorded.jsonl"),
        ]
        .into_iter()
        .enumerate()
        {
            let path = root.join(format!("fixture-{index}.jsonl"));
            std::fs::write(&path, fixture).unwrap();
            smol::block_on(async {
                let handle = agent::claude::start(agent::SessionOptions {
                    cwd: root.clone(),
                    model: Some("claude-opus-5".into()),
                    resume: (index > 0).then(|| {
                        agent::ResumeCursor(serde_json::json!({"session_id":"fixture-session"}))
                    }),
                    fork: false,
                    binary_path: Some(binary.clone()),
                    approval_mode: agent::ApprovalMode::Supervised,
                    option_selections: vec![],
                    interaction_mode: agent::InteractionMode::Build,
                    mcp_servers: vec![],
                    launch_env: agent::LaunchEnv {
                        home: Some(root.clone()),
                        env: vec![
                            (
                                "TCODE_USAGE_FIXTURE".into(),
                                path.to_string_lossy().into_owned(),
                            ),
                            (
                                "TCODE_USAGE_INTERRUPTED".into(),
                                if index == 1 { "1" } else { "0" }.into(),
                            ),
                        ],
                    },
                    extra_args: vec![],
                    acp: None,
                })
                .await
                .unwrap();
                handle
                    .commands
                    .send(agent::SessionCommand::SendTurn {
                        delivery_id: 1,
                        text: "fixture".into(),
                        options: None,
                        attachments: vec![],
                    })
                    .await
                    .unwrap();
                if index == 1 {
                    handle
                        .commands
                        .send(agent::SessionCommand::Interrupt)
                        .await
                        .unwrap();
                }
                loop {
                    let event =
                        smol::future::race(async { handle.events.recv().await.unwrap() }, async {
                            smol::Timer::after(std::time::Duration::from_secs(10)).await;
                            panic!("fixture adapter timed out")
                        })
                        .await;
                    timeline.apply_at(Some(1 + recorded_events.len() as u64), &event);
                    let snapshot =
                        composer_state(Some(&status), Some(&timeline), &settings, &providers);
                    if let agent::AgentEvent::ContextCompacted(c) = &event {
                        let u = snapshot.token_usage.unwrap();
                        if c.in_progress {
                            assert_eq!(u.freshness, agent::ContextFreshness::Compacting);
                        } else {
                            assert_eq!(u.used_tokens, None);
                            assert_eq!(c.pre_tokens, Some(500260));
                            assert_eq!(c.trigger.as_deref(), Some("manual"));
                            assert_eq!(crate::context_meter::used_tokens(&u), None);
                        }
                    }
                    if let agent::AgentEvent::TokenUsage(u) = &event {
                        assert_ne!(
                            u.used_tokens,
                            Some(900000),
                            "subagent context must not enter main usage"
                        );
                        if u.output_tokens == Some(20) {
                            assert_eq!(u.used_tokens, Some(400150));
                        }
                    }
                    let completed = matches!(event, agent::AgentEvent::TurnCompleted { .. });
                    recorded_events.push(event);
                    if completed {
                        break;
                    }
                }
                handle
                    .commands
                    .send(agent::SessionCommand::Shutdown)
                    .await
                    .unwrap();
            });
            let snapshot = composer_state(Some(&status), Some(&timeline), &settings, &providers);
            let usage = snapshot.token_usage.unwrap();
            assert_eq!(usage.used_tokens, Some([500260, 60, 20764][index]));
            assert_eq!(
                usage.total_processed_tokens,
                Some([4001300, 4001365, 4063449][index])
            );
            if index == 1 {
                assert_eq!(
                    timeline.last_turn_status,
                    Some(agent::TurnStatus::Interrupted)
                );
            }
            // A repeated persisted completion is idempotent, including a cancelled result.
            timeline.apply_at(Some(99), recorded_events.last().unwrap());
            assert_eq!(
                timeline.usage.unwrap().total_processed_tokens,
                usage.total_processed_tokens
            );
        }
        for (index, event) in recorded_events.iter().enumerate() {
            replay.apply_at(Some(1 + index as u64), event);
        }
        assert_eq!(replay.usage, timeline.usage);
        let old: agent::AgentEvent = serde_json::from_str(r#"{"type":"token_usage","used_tokens":4100000,"input_tokens":1000000,"context_window":1000000,"total_processed_tokens":4100000}"#).unwrap();
        replay.apply_at(None, &old);
        let old = composer_state(Some(&status), Some(&replay), &settings, &providers)
            .token_usage
            .unwrap();
        assert_eq!(old.freshness, agent::ContextFreshness::Unknown);
        assert_eq!(
            crate::context_meter::used_tokens(&old),
            None,
            "legacy aggregate has no occupancy provenance"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
