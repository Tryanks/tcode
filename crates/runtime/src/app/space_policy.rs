//! What a space's policy lets its members' turns run on. Commands a member
//! sends are checked as they arrive ([`AppState::authorize`]); a turn that
//! leaves the queue later, or that a `tcode_control` call or the host starts
//! during a member's turn, is checked here against the policy as it stands
//! then.
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
    /// The policy of `space_id` as last shown to the host.
    pub(crate) fn space_policy(&self, space_id: &str) -> Option<&SpacePolicy> {
        self.space_policies.get(space_id)
    }

    /// The thread's current setup: the resident copy, which a live change
    /// reaches first, or the stored one.
    pub(super) fn current_meta(&self, session_id: &str) -> Option<&SessionMeta> {
        self.resident(session_id)
            .map(|session| &session.meta)
            .or_else(|| self.sessions.by_id(session_id))
    }

    /// The model a thread runs: its own choice, or its profile's default.
    pub(super) fn effective_model(&self, meta: &SessionMeta) -> Option<String> {
        meta.model.clone().or_else(|| {
            self.models_for(meta.provider)
                .iter()
                .find(|model| model.is_default)
                .map(|model| model.id.clone())
        })
    }

    /// Whether `policy` lets a turn run on `meta`'s account profile, model,
    /// ACP agent and permission value.
    pub(super) fn setup_allowed(
        &self,
        policy: &SpacePolicy,
        meta: &SessionMeta,
    ) -> Result<(), ProtocolError> {
        let profile = meta
            .profile_id
            .as_deref()
            .unwrap_or_else(|| Settings::builtin_profile_id(meta.provider));
        let access = policy.agent(profile).ok_or_else(|| {
            policy_error(format!(
                "this space does not allow {}",
                self.profile_display_name(profile)
            ))
        })?;
        let model = self.effective_model(meta);
        if !access.allows_model(model.as_deref()) {
            return Err(policy_error(format!(
                "this space does not allow the model {}",
                model.as_deref().unwrap_or("the profile's default")
            )));
        }
        if meta.provider == ProviderKind::Acp
            && !access.allows_acp_agent(meta.acp_agent_id.as_deref())
        {
            return Err(policy_error("this space does not allow that ACP agent"));
        }
        let class = agent::session_approval_class(meta.provider, &meta.option_selections);
        if !policy.allows_class(class) {
            return Err(policy_error(approval_refusal(policy.approval)));
        }
        Ok(())
    }

    /// [`Self::setup_allowed`] for one thread, by id.
    pub(super) fn session_setup_allowed(
        &self,
        policy: &SpacePolicy,
        session_id: &str,
    ) -> Result<(), ProtocolError> {
        let meta = self
            .current_meta(session_id)
            .ok_or_else(|| policy_error("unknown thread"))?;
        self.setup_allowed(policy, meta)
    }

    /// Whether a member may switch a thread to `provider`/`profile_id`/`model`.
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
    /// the thread's setup as it stands, and release the rest. A held message
    /// keeps its place and goes once the thread or the space allows it.
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

    /// After a space's policy changed: hold or release every queued message
    /// under it and send what may go now.
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

    /// Whether `message` must wait: the space it goes under (its own, or for
    /// one the host or an agent wrote, the thread's latest turn's) refuses the
    /// thread's setup, or is not known to this host.
    pub(super) fn queued_held(&self, session: &ActiveSession, message: &QueuedMessage) -> bool {
        let space = match message.origin {
            MessageOrigin::Human => message.space.as_deref(),
            MessageOrigin::Agent | MessageOrigin::Server => {
                message.space.as_deref().or(session.turn_space.as_deref())
            }
        };
        space.is_some_and(|space| {
            self.space_policy(space)
                .is_none_or(|policy| self.setup_allowed(policy, &session.meta).is_err())
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

    /// Let the thread's computer use answer only when the space its turn runs
    /// under allows it: a member's turn must not operate the machine's desktop
    /// unless the owner said so.
    pub(super) fn apply_turn_tools(&self, session_id: &str) {
        let Some(tokens) = &self.mcp.computer_use_tokens else {
            return;
        };
        if self
            .turn_policy(session_id)
            .is_some_and(|policy| !policy.computer_use)
        {
            tokens.suspend(session_id);
        } else {
            tokens.resume(session_id);
        }
    }

    /// The policy the thread's current turn runs under, when a member's.
    pub(super) fn turn_policy(&self, session_id: &str) -> Option<&SpacePolicy> {
        let space = self.resident(session_id)?.turn_space.as_deref()?;
        self.space_policy(space)
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

    /// The space whose policy a `tcode_control` call made by `thread_id` runs
    /// under: that of the turn making it.
    pub(super) fn control_space(&self, thread_id: &str) -> Option<String> {
        self.resident(thread_id)?.turn_space.clone()
    }

    /// The projects of `space_id` as last shown to the host.
    pub(super) fn space_projects_of(&self, space_id: &str) -> Option<&BTreeSet<String>> {
        self.space_scopes.get(space_id)
    }

    /// Whether `policy` lets members start a thread on this setup, choosing
    /// for it a permission value the policy allows when the one it was given
    /// is not: the provider's recommended value, then its default, then the
    /// first it lists that is allowed.
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
            .and_then(|space| self.space_policy(space))
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
        let profile = meta
            .profile_id
            .as_deref()
            .unwrap_or_else(|| Settings::builtin_profile_id(meta.provider));
        let model_allowed = policy
            .agent(profile)
            .is_some_and(|access| access.allows_model(self.effective_model(&meta).as_deref()));
        if !model_allowed
            && let Some(choice) = self.scoped_provider_choices(&policy).into_iter().next()
        {
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
