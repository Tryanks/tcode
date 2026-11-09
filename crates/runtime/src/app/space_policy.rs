//! What a space's policy lets its members' turns run on. Commands a member
//! sends are checked as they arrive ([`AppState::authorize`]); a turn that
//! leaves the queue later, or that a `tcode_control` call or the host starts
//! during a member's turn, is checked here against the policy as it stands
//! then. A turn keeps the policy it started under until it ends.
use super::*;
use tcode_protocol::{SpaceApproval, SpacePolicy};

/// Why a space's policy refuses a setup.
pub(crate) fn policy_error(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: "space_policy".into(),
        message: message.into(),
    }
}

impl AppState {
    /// The thread's current setup: the resident copy, which a live change
    /// reaches first, or the stored one ([`Self::find_meta`] prefers the
    /// stored copy).
    pub(super) fn current_meta(&self, session_id: &str) -> Option<&SessionMeta> {
        self.resident(session_id)
            .map(|session| &session.meta)
            .or_else(|| self.sessions.by_id(session_id))
    }

    /// Whether `policy` lets a turn run on `meta`'s account profile, model,
    /// ACP agent and permission value.
    pub(super) fn setup_allowed(
        &self,
        policy: &SpacePolicy,
        meta: &SessionMeta,
    ) -> Result<(), ProtocolError> {
        self.model_allowed(
            policy,
            meta.provider,
            meta.profile_id.as_deref(),
            meta.model.as_deref(),
        )?;
        let profile = meta
            .profile_id
            .as_deref()
            .unwrap_or_else(|| Settings::builtin_profile_id(meta.provider));
        if meta.provider == ProviderKind::Acp
            && !policy
                .agent(profile)
                .is_some_and(|access| access.allows_acp_agent(meta.acp_agent_id.as_deref()))
        {
            return Err(policy_error("this space does not allow that ACP agent"));
        }
        let class = agent::session_approval_class(meta.provider, &meta.option_selections);
        if !policy.allows_class(class) {
            return Err(policy_error(approval_refusal(policy.approval)));
        }
        Ok(())
    }

    /// Whether `policy` lets a turn go out on `session` now: its setup, and
    /// while a restart waits (on background work or a rewind), the setup its
    /// live process still runs, which a message sent meanwhile would get.
    pub(super) fn session_runs_under(
        &self,
        policy: &SpacePolicy,
        session: &ActiveSession,
    ) -> Result<(), ProtocolError> {
        self.setup_allowed(policy, &session.meta)?;
        if matches!(session.runtime, Runtime::Live(_)) && session.restart_required() {
            let mut live = session.meta.clone();
            live.model = session.live_model.clone();
            live.profile_id = session.live_profile_id.clone();
            live.option_selections = session.live_option_selections.clone();
            self.setup_allowed(policy, &live)?;
        }
        Ok(())
    }

    /// [`Self::session_runs_under`] for one thread, by id; a thread that is
    /// not resident runs on its stored setup.
    pub(super) fn session_setup_allowed(
        &self,
        policy: &SpacePolicy,
        session_id: &str,
    ) -> Result<(), ProtocolError> {
        match self.resident(session_id) {
            Some(session) => self.session_runs_under(policy, session),
            None => self.setup_allowed(
                policy,
                self.sessions
                    .by_id(session_id)
                    .ok_or_else(|| policy_error("unknown thread"))?,
            ),
        }
    }

    /// Whether `policy` lets members run `provider`'s `profile_id` on `model`
    /// (the provider's default model when `None`).
    pub(super) fn model_allowed(
        &self,
        policy: &SpacePolicy,
        provider: ProviderKind,
        profile_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), ProtocolError> {
        let profile = profile_id.unwrap_or_else(|| Settings::builtin_profile_id(provider));
        let access = policy.agent(profile).ok_or_else(|| {
            policy_error(format!(
                "this space does not allow {}",
                self.profile_display_name(profile)
            ))
        })?;
        let model = model.map(str::to_owned).or_else(|| {
            self.models_for(provider)
                .iter()
                .find(|spec| spec.is_default)
                .map(|spec| spec.id.clone())
        });
        if access.allows_model(model.as_deref()) {
            Ok(())
        } else {
            Err(policy_error(format!(
                "this space does not allow the model {}",
                model.as_deref().unwrap_or("the profile's default")
            )))
        }
    }

    /// Whether a member may set option `id` of a thread on `provider` to
    /// `value` (its default when `None`).
    pub(super) fn option_allowed(
        &self,
        policy: &SpacePolicy,
        provider: ProviderKind,
        id: &str,
        value: Option<&serde_json::Value>,
    ) -> Result<(), ProtocolError> {
        if !agent::option_governs_approvals(provider, id) {
            return Ok(());
        }
        let value = match value {
            Some(value) => value.as_str().map(str::to_owned),
            None => match agent::permission_control(provider) {
                Some(agent::OptionDescriptor::Select { default_value, .. }) => default_value,
                _ => None,
            },
        };
        let class = value
            .and_then(|value| agent::approval_class(provider, id, &value))
            .unwrap_or(agent::ApprovalClass::Unrestricted);
        if policy.allows_class(class) {
            Ok(())
        } else {
            Err(policy_error(approval_refusal(policy.approval)))
        }
    }

    /// Hold every queued message of `session_id` whose space's policy refuses
    /// the thread as it stands, and release the rest. A held message keeps its
    /// place and goes once the thread or the space allows it.
    pub(super) fn refresh_policy_holds(&mut self, session_id: &str) {
        let Some(session) = self.resident(session_id) else {
            return;
        };
        let holds: Vec<(u64, bool)> = session
            .queue
            .iter()
            .map(|message| (message.id, self.queued_held(session, message)))
            .collect();
        if let Some(session) = self.resident_mut(session_id) {
            for (id, held) in holds {
                if let Some(message) = session.queue.iter_mut().find(|m| m.id == id) {
                    message.space_held = held;
                }
            }
        }
    }

    /// After a space's policy, or anything a thread's setup is judged by,
    /// changed: hold or release every queued message and send what may go.
    pub(crate) fn space_policy_changed(&mut self, cx: &mut HostCx) {
        let ids: Vec<String> = self
            .residents
            .live
            .iter()
            .chain(self.residents.parked.iter())
            .filter(|(_, session)| !session.queue.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.refresh_policy_holds(&id);
            if self.dispatch_next_queued(&id, cx).is_err() {
                log::warn!("{id}: dispatch after a space policy change failed (process gone)");
            }
        }
    }

    /// The space `message` goes under: its own, or for one the host or an
    /// agent wrote without one, the thread's latest turn's.
    fn message_space<'a>(
        session: &'a ActiveSession,
        message: &'a QueuedMessage,
    ) -> Option<&'a str> {
        match message.origin {
            MessageOrigin::Human => message.space.as_deref(),
            MessageOrigin::Agent | MessageOrigin::Server => {
                message.space.as_deref().or(session.turn_space.as_deref())
            }
        }
    }

    /// Whether `message` must wait: the space it goes under refuses the
    /// thread as it stands, or is not known to this host.
    pub(super) fn queued_held(&self, session: &ActiveSession, message: &QueuedMessage) -> bool {
        Self::message_space(session, message).is_some_and(|space| {
            self.space_policies
                .get(space)
                .is_none_or(|policy| self.session_runs_under(policy, session).is_err())
        })
    }

    /// After a thread's setup changed: send a message it was holding, when
    /// the change lets one go.
    pub(crate) fn release_policy_holds(&mut self, session_id: &str, cx: &mut HostCx) {
        let holding = self
            .resident(session_id)
            .is_some_and(|session| session.queue.iter().any(|message| message.space_held));
        if holding && self.dispatch_next_queued(session_id, cx).is_err() {
            log::warn!("{session_id}: dispatch after a setup change failed (process gone)");
        }
    }

    /// Put the thread's turn under `space` (a member's, or `None` for the
    /// machine's own user), with that space's policy as it stands now, which
    /// the turn keeps until the next one starts.
    pub(super) fn enter_turn_space(&mut self, session_id: &str, space: Option<String>) {
        let policy = space.as_deref().map(|id| {
            self.space_policies
                .get(id)
                .cloned()
                .unwrap_or_else(SpacePolicy::new_space)
        });
        let Some(session) = self.resident_mut(session_id) else {
            return;
        };
        session.turn_space = space;
        session.turn_policy = policy;
        self.apply_turn_tools(session_id);
    }

    /// Let the thread's computer use and preview tools answer only when the
    /// policy its turn runs under allows them: a member's turn operates the
    /// machine's desktop or drives its preview browser only if the owner
    /// said so.
    fn apply_turn_tools(&self, session_id: &str) {
        let policy = self.turn_policy(session_id);
        if let Some(tokens) = &self.mcp.computer_use_tokens {
            if policy.is_some_and(|policy| !policy.computer_use) {
                tokens.suspend(session_id);
            } else {
                tokens.resume(session_id);
            }
        }
        if let Some(tokens) = &self.mcp.preview_tokens {
            if policy.is_some_and(|policy| !policy.preview) {
                tokens.suspend(session_id);
            } else {
                tokens.resume(session_id);
            }
        }
    }

    /// The policy the thread's current turn runs under, when a member's: the
    /// space's policy as it stood when the turn started.
    pub(super) fn turn_policy(&self, session_id: &str) -> Option<&SpacePolicy> {
        self.resident(session_id)?.turn_policy.as_ref()
    }

    /// Decline at once an approval the harness asks during a turn whose space
    /// allows only approvals the harness settles itself. Leaving plan mode is
    /// not a permission, and is left for a person. `true` when declined.
    pub(super) fn decline_for_space(
        &mut self,
        session_id: &str,
        request: &agent::ApprovalRequest,
    ) -> bool {
        if matches!(request.kind, agent::ApprovalKind::Plan { .. })
            || self
                .turn_policy(session_id)
                .is_none_or(|policy| policy.approval != SpaceApproval::ReviewedOnly)
        {
            return false;
        }
        let decision = request
            .options
            .iter()
            .find(|option| option.kind == agent::ApprovalOptionKind::RejectOnce)
            .map_or(agent::ApprovalDecision::Cancel, |option| {
                agent::ApprovalDecision::Option(option.id.clone())
            });
        match self.respond_session_approval(session_id, request.id.clone(), decision) {
            Ok(()) => true,
            Err(error) => {
                log::warn!("could not decline {session_id}'s approval for its space: {error}");
                false
            }
        }
    }

    /// The space whose member's turn `thread_id` is working in, if any.
    pub(super) fn turn_space_of(&self, thread_id: &str) -> Option<String> {
        self.resident(thread_id)?.turn_space.clone()
    }

    /// Whether `policy` lets members start a thread on this setup, choosing
    /// for it a permission value the policy allows when the one it was given
    /// is not ([`allowed_permission`]).
    pub(super) fn fit_to_policy(
        &self,
        policy: &SpacePolicy,
        meta: &mut SessionMeta,
    ) -> Result<(), ProtocolError> {
        let class = agent::session_approval_class(meta.provider, &meta.option_selections);
        if !policy.allows_class(class)
            && let Some(selection) = allowed_permission(policy, meta.provider)
        {
            meta.option_selections
                .retain(|chosen| chosen.id != selection.id);
            meta.option_selections.push(selection);
        }
        self.setup_allowed(policy, meta)
    }

    /// A draft a member started takes the machine's defaults, which the space
    /// may not allow: move it to the first agent and model the space allows,
    /// and to a permission mode it allows, so the member's first message can go.
    pub(crate) fn fit_draft_to_space(&mut self, session_id: &str, cx: &mut HostCx) {
        let Some(policy) = cx
            .space
            .as_deref()
            .and_then(|space| self.space_policies.get(space))
            .cloned()
        else {
            return;
        };
        let Some(meta) = self.current_meta(session_id).cloned() else {
            return;
        };
        if self.setup_allowed(&policy, &meta).is_ok() {
            return;
        }
        let runs = self
            .model_allowed(
                &policy,
                meta.provider,
                meta.profile_id.as_deref(),
                meta.model.as_deref(),
            )
            .is_ok()
            && policy.allows_provider(meta.provider);
        if !runs && let Some(choice) = self.scoped_provider_choices(&policy).into_iter().next() {
            let model = choice
                .models
                .iter()
                .find(|model| model.is_default)
                .or(choice.models.first())
                .map(|model| model.id.clone());
            let builtin = choice
                .profile_id
                .as_deref()
                .is_some_and(Settings::is_builtin_profile_id);
            self.set_active_model(
                session_id,
                choice.provider,
                model,
                choice.profile_id.filter(|_| !builtin),
                cx,
            );
        }
        let Some(meta) = self.current_meta(session_id) else {
            return;
        };
        let class = agent::session_approval_class(meta.provider, &meta.option_selections);
        if !policy.allows_class(class)
            && let Some(selection) = allowed_permission(&policy, meta.provider)
        {
            self.set_active_option(session_id, &selection.id, Some(selection.value), cx);
        }
    }
}

/// A value of `provider`'s permission control that `policy` allows: its
/// recommended value, then its default, then the first it lists.
fn allowed_permission(policy: &SpacePolicy, provider: ProviderKind) -> Option<OptionSelection> {
    let agent::OptionDescriptor::Select {
        id,
        options,
        default_value,
        recommended,
        ..
    } = agent::permission_control(provider)?
    else {
        return None;
    };
    let value = recommended
        .into_iter()
        .chain(default_value)
        .chain(options.into_iter().map(|option| option.value))
        .find(|value| {
            agent::approval_class(provider, &id, value)
                .is_some_and(|class| policy.allows_class(class))
        })?;
    Some(OptionSelection {
        id,
        value: serde_json::Value::String(value),
    })
}

fn approval_refusal(approval: SpaceApproval) -> &'static str {
    match approval {
        SpaceApproval::Any => "this space allows any permission mode",
        SpaceApproval::NoUnrestricted => {
            "this space does not allow a permission mode that runs everything unasked"
        }
        SpaceApproval::ReviewedOnly => {
            "this space allows only permission modes in which the agent's harness settles approvals itself"
        }
    }
}
