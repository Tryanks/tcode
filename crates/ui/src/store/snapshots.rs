use std::path::PathBuf;

use tcode_core::{
    project::WorktreeInfo,
    session::PendingUserInput,
    settings::Settings,
    ui::{RightTab, WorkspaceMode},
};
use tcode_protocol::{ProvidersStatus, QueuedMessageStatus, SessionStatus, TerminalContextStatus};

use crate::conversation_ui::ConversationUiState;

#[derive(Clone)]
pub struct ComposerActiveModel {
    pub provider: agent::ProviderKind,
    pub model: Option<String>,
    pub acp_agent_id: Option<String>,
    pub profile_id: Option<String>,
}

#[derive(Clone, PartialEq)]
pub struct ComposerCheckoutState {
    pub branch: String,
    pub branches: Vec<String>,
    pub checkout_blocked: bool,
    pub is_draft: bool,
    pub worktree_base: Option<String>,
    pub worktree: Option<WorktreeInfo>,
}

#[derive(Clone)]
pub struct ComposerQueue {
    pub messages: Vec<QueuedMessageStatus>,
    pub can_steer: bool,
    pub agent: &'static str,
    /// Held after a rewind until the user sends the next message.
    pub held: bool,
}

/// The complete replica-derived state consumed by composer views in one frame.
#[derive(Clone)]
pub struct ComposerState {
    pub has_active_session: bool,
    pub terminal_contexts: Vec<TerminalContextStatus>,
    pub relay_confirmation: Option<(String, String)>,
    pub active_cwd: Option<PathBuf>,
    pub provider_commands: Vec<agent::ProviderCommand>,
    pub attachments_dir: Option<PathBuf>,
    pub pending_user_input: Option<PendingUserInput>,
    pub active_model: Option<ComposerActiveModel>,
    pub model_pending_restart: bool,
    pub active_model_spec: Option<agent::ModelSpec>,
    pub active_option_descriptors: Vec<agent::OptionDescriptor>,
    pub active_option_selections: Vec<agent::OptionSelection>,
    pub requested_option_selections: Vec<agent::OptionSelection>,
    pub options_pending_restart: bool,

    pub token_usage: Option<agent::TokenUsage>,
    pub goal: Option<agent::Goal>,
    /// Account rate-limit windows for the profile driving this session.
    pub usage: Option<tcode_core::usage::ProviderUsage>,
    pub provider: Option<agent::ProviderKind>,
    pub queue: Option<ComposerQueue>,
    pub steering_supported: bool,
    pub preparing_worktree: bool,
    pub checkout: Option<ComposerCheckoutState>,
    pub turn_running: bool,
    pub stopping: bool,
    pub context_window: Option<u64>,
    pub checkout_blocked: bool,
    pub conversation_read_only: bool,
    pub terminal_limit_reached: bool,
    pub terminal_split_available: bool,
    /// The first approval the composer asks; a plan's is asked on its block.
    pub pending_approval: Option<agent::ApprovalRequest>,
    pub pending_approval_count: usize,
    /// The plan decision the agent waits on, which a send answers in words.
    pub pending_plan: Option<agent::ApprovalRequest>,
    /// The policy of the space this composer's workspace is a member of.
    pub space_policy: Option<tcode_protocol::SpacePolicy>,
}

pub(crate) fn composer_state(
    status: Option<&SessionStatus>,
    settings: &Settings,
    providers: &ProvidersStatus,
) -> ComposerState {
    let provider = status.map(|status| status.provider);
    let approvals = || {
        status
            .into_iter()
            .flat_map(|status| &status.pending_approvals)
    };
    let is_plan =
        |request: &agent::ApprovalRequest| matches!(request.kind, agent::ApprovalKind::Plan { .. });
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
            checkout_blocked: status.checkout_blocked,
            is_draft: status.draft,
            worktree_base: match &status.draft_workspace {
                WorkspaceMode::NewWorktree { base } => Some(base.clone()),
                _ => None,
            },
            worktree: status.worktree.clone(),
        })
    });
    let token_usage = status.and_then(|status| {
        status.usage.map(|mut usage| {
            usage.context_window = status.context_window;
            usage
        })
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
        space_policy: None,
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
        pending_user_input: status.and_then(|status| status.pending_user_input.clone()),
        active_model: status.map(|status| ComposerActiveModel {
            provider: status.provider,
            model: status.requested_model.clone(),
            acp_agent_id: status.acp_agent_id.clone(),
            profile_id: status.requested_profile_id.clone(),
        }),
        model_pending_restart: status.is_some_and(|status| status.model_pending_restart),
        active_model_spec,
        active_option_descriptors: status
            .map(|status| {
                status
                    .provider_option_descriptors
                    .iter()
                    .cloned()
                    .map(|mut descriptor| {
                        let (agent::OptionDescriptor::Select { label, .. }
                        | agent::OptionDescriptor::Boolean { label, .. }) = &mut descriptor;
                        *label = crate::i18n::translate_option_name(label);
                        descriptor
                    })
                    .collect()
            })
            .unwrap_or_default(),
        active_option_selections: status
            .map(|status| status.provider_option_selections.clone())
            .unwrap_or_default(),
        requested_option_selections: status
            .map(|status| status.provider_option_requested_selections.clone())
            .unwrap_or_default(),
        options_pending_restart: status.is_some_and(|status| status.options_pending_restart),
        token_usage,
        goal: status.and_then(|status| status.goal.clone()),
        usage,
        provider,
        queue: status.map(|status| ComposerQueue {
            messages: status.queued_messages.clone(),
            can_steer: status.steering_supported,
            agent: status.provider.display_name(),
            held: status.queue_held,
        }),
        steering_supported: status.is_some_and(|status| status.steering_supported),
        preparing_worktree: status.is_some_and(|status| status.preparing_worktree),
        checkout,
        turn_running: status.is_some_and(|status| status.activity.turn_running),
        stopping: status.is_some_and(|status| status.stopping),
        context_window: status.and_then(|status| status.context_window),
        checkout_blocked: status.is_none_or(|status| status.checkout_blocked),
        conversation_read_only: status.is_some_and(|status| status.conversation_read_only),
        terminal_limit_reached: status.is_some_and(|status| status.terminal_limit_reached),
        terminal_split_available: status.is_some_and(|status| status.terminal_split_available),
        pending_approval: approvals().find(|request| !is_plan(request)).cloned(),
        pending_approval_count: approvals().filter(|request| !is_plan(request)).count(),
        pending_plan: approvals().find(|request| is_plan(request)).cloned(),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PanelState {
    pub right_panel_open: bool,
    pub right_tab: RightTab,
    pub right_panel_expanded: bool,
    pub terminal_open: bool,
    pub terminal_height: f32,
}

pub(crate) fn panel_state(ui: Option<&ConversationUiState>) -> PanelState {
    PanelState {
        right_panel_open: ui.is_some_and(|ui| ui.right_panel_open),
        right_tab: ui.map_or_else(RightTab::default, |ui| ui.right_tab),
        right_panel_expanded: ui.is_some_and(|ui| ui.right_panel_expanded),
        terminal_open: ui.is_some_and(|ui| ui.terminal_open),
        terminal_height: ui.map_or(240., |ui| ui.terminal_height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcode_core::session::Timeline;

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
            queued_messages: Vec::new(),
            background_tasks: Vec::new(),
            review_comment_drafts: Vec::new(),
            terminals: Vec::new(),
            active_terminal_id: None,
            terminal_splits: Vec::new(),
            terminal_contexts: Vec::new(),
            terminal_open: false,
            terminal_height: 240.,
            delivery_in_flight: None,
            activity: tcode_protocol::SessionActivity {
                working: false,
                turn_running: false,
                background: Default::default(),
                unsettled: 0,
                wakes: Default::default(),
                waiting_for_approval: false,
                waiting_for_input: false,
                settle_blocked: false,
                failed: false,
                unread: false,
                fork: tcode_protocol::ForkAvailability::Available,
                agent: None,
                latest_activity: None,
            },
            stopping: false,
            stopping_since: None,
            turn_retry: None,
            goal: None,
            rewind: None,
            fork_at: None,
            queue_held: false,
            checkout_blocked: false,
            conversation_read_only: false,
            terminal_limit_reached: false,
            terminal_split_available: false,
            usage: None,
            context_window: None,
            running_turn: None,
            pending_approvals: Vec::new(),
            pending_user_input: None,
            steering_supported: true,
            provider_option_descriptors: Vec::new(),
            provider_option_selections: Vec::new(),
            provider_option_requested_selections: Vec::new(),
            provider_commands: Vec::new(),
            git_branch: Some("main".into()),
            branches: vec!["main".into()],
            draft: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            worktree: None,
            preparing_worktree: false,
            relay_confirmation: None,
            model_pending_restart: false,
            options_pending_restart: false,
            injected_tools: None,
            control: None,
        }
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
        status.usage = timeline.usage;
        let custom = composer_state(Some(&status), &settings, &providers);
        assert_eq!(custom.token_usage.unwrap().used_tokens, Some(1234));
        assert!(custom.usage.is_none());
        settings
            .profiles
            .get_mut("custom")
            .unwrap()
            .settings
            .env
            .clear();
        let native = composer_state(Some(&status), &settings, &providers);
        assert_eq!(
            native.usage.unwrap().error.as_deref(),
            Some("temporarily unreachable")
        );
        assert_eq!(native.token_usage.unwrap().used_tokens, Some(1234));
    }
}
