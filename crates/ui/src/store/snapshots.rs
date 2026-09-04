use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use tcode_core::{
    project::WorktreeInfo,
    provider_models::{ResolvedModel, picker_models},
    session::Timeline,
    settings::Settings,
    ui::{RightTab, WorkspaceMode},
};
use tcode_protocol::{ProvidersStatus, QueuedMessageStatus, SessionStatus};

use crate::conversation_ui::ConversationUiState;

#[derive(Clone)]
pub(crate) struct ComposerTerminalContext {
    pub id: u64,
    pub terminal_label: String,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

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
    pub terminal_contexts: Vec<ComposerTerminalContext>,
    pub relay_confirmation: Option<(String, String)>,
    pub active_cwd: Option<PathBuf>,
    pub provider_commands: Vec<agent::ProviderCommand>,
    pub attachments_dir: Option<PathBuf>,
    pub pending_user_input: Option<(String, Vec<agent::UserInputQuestion>)>,
    pub active_model: Option<ComposerActiveModel>,
    picker_models: HashMap<agent::ProviderKind, Vec<ResolvedModel>>,
    models_loading: HashSet<agent::ProviderKind>,
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

impl ComposerState {
    pub fn picker_models(&self, provider: agent::ProviderKind) -> Vec<ResolvedModel> {
        self.picker_models
            .get(&provider)
            .cloned()
            .unwrap_or_default()
    }

    pub fn models_loading(&self, provider: agent::ProviderKind) -> bool {
        self.models_loading.contains(&provider)
    }
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
    let picker_models = [
        agent::ProviderKind::Codex,
        agent::ProviderKind::ClaudeCode,
        agent::ProviderKind::Pi,
        agent::ProviderKind::OpenCode,
        agent::ProviderKind::Acp,
    ]
    .into_iter()
    .map(|provider| {
        let catalog = providers
            .model_catalogs
            .get(&provider)
            .map_or(&[][..], Vec::as_slice);
        (
            provider,
            picker_models(
                catalog,
                &settings.provider(provider),
                &settings.favorite_models,
            ),
        )
    })
    .collect();
    let models_loading = providers
        .models_loading
        .iter()
        .filter_map(|(&provider, &loading)| {
            (loading
                && providers
                    .model_catalogs
                    .get(&provider)
                    .is_none_or(Vec::is_empty))
            .then_some(provider)
        })
        .collect();
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
        providers.provider_usage.get(&profile_id).cloned()
    });

    ComposerState {
        has_active_session: status.is_some(),
        terminal_contexts: status
            .map(|status| {
                status
                    .terminal_contexts
                    .iter()
                    .map(|context| ComposerTerminalContext {
                        id: context.id,
                        terminal_label: context.terminal_label.clone(),
                        line_start: context.line_start,
                        line_end: context.line_end,
                        text: context.text.clone(),
                    })
                    .collect()
            })
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
        picker_models,
        models_loading,
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
pub(crate) struct ChatPanelState {
    pub right_panel_open: bool,
    pub right_tab: RightTab,
    pub plan_showing: bool,
    pub preview_showing: bool,
    pub terminal_open: bool,
    pub terminal_height: f32,
}

pub(crate) fn chat_panel_state(ui: Option<&ConversationUiState>) -> ChatPanelState {
    let right_panel_open = ui.is_some_and(|ui| ui.right_panel_open);
    let right_tab = ui.map_or_else(RightTab::default, |ui| ui.right_tab);
    ChatPanelState {
        right_panel_open,
        right_tab,
        plan_showing: right_panel_open && right_tab == RightTab::Plan,
        preview_showing: right_panel_open && right_tab == RightTab::Preview,
        terminal_open: ui.is_some_and(|ui| ui.terminal_open),
        terminal_height: ui.map_or(240., |ui| ui.terminal_height),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ShellPanelState {
    pub right_panel_open: bool,
    pub right_tab: RightTab,
    pub right_panel_expanded: bool,
}

pub(crate) fn shell_panel_state(ui: Option<&ConversationUiState>) -> ShellPanelState {
    ShellPanelState {
        right_panel_open: ui.is_some_and(|ui| ui.right_panel_open),
        right_tab: ui.map_or_else(RightTab::default, |ui| ui.right_tab),
        right_panel_expanded: ui.is_some_and(|ui| ui.right_panel_expanded),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DiffPanelChrome {
    pub panel_open: bool,
    pub expanded: bool,
    pub active_tab: RightTab,
    pub plan_tab_active: bool,
}

pub(crate) fn diff_panel_chrome(
    ui: Option<&ConversationUiState>,
    status: Option<&SessionStatus>,
    timeline: Option<&Timeline>,
) -> DiffPanelChrome {
    DiffPanelChrome {
        panel_open: ui.is_some_and(|ui| ui.right_panel_open),
        expanded: ui.is_some_and(|ui| ui.right_panel_expanded),
        active_tab: ui.map_or_else(RightTab::default, |ui| ui.right_tab),
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
    fn composer_state_distinguishes_active_and_missing_sessions() {
        let settings = Settings::default();
        let providers = ProvidersStatus::default();

        let missing = composer_state(None, None, &settings, &providers);
        assert!(!missing.has_active_session);
        assert!(missing.active_model.is_none());
        assert!(missing.queue.is_none());

        let status = session_status();
        let active = composer_state(Some(&status), None, &settings, &providers);
        assert!(active.has_active_session);
        assert_eq!(active.active_cwd, Some(PathBuf::from("/workspace")));
        assert_eq!(
            active.active_model.as_ref().map(|model| model.provider),
            Some(agent::ProviderKind::Codex)
        );
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
    fn composer_state_carries_queued_messages_and_steering_capability() {
        let mut status = session_status();
        status.queued_messages = vec![QueuedMessageStatus {
            id: 7,
            text: "follow up".into(),
            fire_at_unix_secs: None,
        }];
        status.steering_supported = false;

        let state = composer_state(
            Some(&status),
            None,
            &Settings::default(),
            &ProvidersStatus::default(),
        );
        let queue = state.queue.expect("active sessions have queue state");
        assert_eq!(queue.messages, status.queued_messages);
        assert!(!queue.can_steer);
        assert_eq!(queue.agent, "Codex");
    }

    #[test]
    fn chat_panel_state_derives_expansion_flags() {
        let mut ui = ConversationUiState::new(false, true, 320.);
        ui.right_panel_open = true;
        ui.right_tab = RightTab::Plan;

        let plan = chat_panel_state(Some(&ui));
        assert!(plan.plan_showing);
        assert!(!plan.preview_showing);
        assert!(plan.terminal_open);
        assert_eq!(plan.terminal_height, 320.);

        ui.right_tab = RightTab::Preview;
        let preview = chat_panel_state(Some(&ui));
        assert!(!preview.plan_showing);
        assert!(preview.preview_showing);

        ui.right_panel_open = false;
        let closed = chat_panel_state(Some(&ui));
        assert!(!closed.plan_showing);
        assert!(!closed.preview_showing);
    }
}
