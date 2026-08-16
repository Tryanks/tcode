use super::*;

impl AppState {
    /// Select a provider-owned `model` (None = provider default) for the active
    /// session and persist it. On an unsent draft the model picker also selects
    /// its provider; an established session remains bound to its provider.
    /// Takes effect on the next provider (re)start; if a provider is currently
    /// live, the next `send_turn` restarts it (see `send_turn`).
    pub fn set_active_model(
        &mut self,
        provider: ProviderKind,
        model: Option<String>,
        // Which provider profile the picked row belongs to (`None` = the built-in
        // profile for `provider`). Rebinding an established session's profile is
        // a backend change and goes through the relay confirmation, exactly like
        // a provider change.
        profile_id: Option<String>,
        cx: &mut HostCx,
    ) {
        let profile_id = profile_id.filter(|id| !Settings::is_builtin_profile_id(id));
        let store = self.store.clone();
        let Some(active) = self.residents.active.as_mut() else {
            return;
        };
        // In a draft the model picker is also the provider picker. The selected
        // row carries its provider explicitly: model ids are provider-defined
        // and custom ids cannot be classified safely from their spelling.
        if active.draft {
            if active.meta.provider == provider
                && active.meta.model == model
                && active.meta.profile_id == profile_id
            {
                return;
            }
            active.meta.provider = provider;
            active.meta.acp_agent_id = None;
            active.meta.profile_id = profile_id;
            active.meta.model = model;
            // A different model has different option descriptors: drop stale
            // selections so each resolves to the new model's defaults.
            active.meta.option_selections.clear();
            active.provider_commands = store.load_commands(active.meta.provider, None);
            active.pending_ultrathink = false;
            return;
        }
        // Established sessions can preview a different provider — or a
        // different profile of the same provider, which is a different backend
        // with its own isolated home — but the provider-native cursor is
        // retained until the user confirms a relay.
        if active.meta.provider != provider || active.meta.profile_id != profile_id {
            let source = active.pending_relay.clone().unwrap_or(PendingRelay {
                from_provider: active.meta.provider,
                from_model: active.meta.model.clone(),
                from_profile: active.meta.profile_id.clone(),
            });
            if active.pending_relay.is_some()
                && source.from_provider == provider
                && source.from_profile == profile_id
            {
                active.pending_relay = None;
            } else if has_meaningful_history(&active.timeline) {
                active.pending_relay = Some(source);
            } else {
                active.resume_cursor_for_fresh_provider();
            }
            active.meta.provider = provider;
            active.meta.acp_agent_id = None;
            active.meta.profile_id = profile_id;
            active.meta.model = model;
            active.meta.option_selections.clear();
            active.provider_commands = store.load_commands(provider, None);
            active.provider_options.clear();
            active.pending_ultrathink = false;
            if active.pending_relay.is_some() {
                return;
            }
            self.preview_draft_or_persist_active(cx);
            return;
        }
        if active.meta.model == model {
            return;
        }
        active.meta.model = model;
        active.meta.option_selections.clear();
        active.pending_ultrathink = false;
        if active.pending_relay.is_some() {
            return;
        }
        self.preview_draft_or_persist_active(cx);
    }

    // -- traits (option selections) -----------------------------------------

    /// Set (or clear) the persisted value of one option descriptor for the
    /// active session. `value` is a string (select) or bool (boolean); passing
    /// `None` removes the selection so it resolves back to its default. Takes
    /// effect per the restart machinery (see `send_turn`).
    pub fn set_active_option(
        &mut self,
        id: &str,
        value: Option<serde_json::Value>,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.residents.active.as_mut() else {
            return;
        };
        active.meta.option_selections.retain(|s| s.id != id);
        if let Some(value) = value {
            active.meta.option_selections.push(OptionSelection {
                id: id.to_string(),
                value,
            });
        }
        // Selecting a real reasoning effort supersedes a pending Ultrathink.
        if id == "reasoningEffort" {
            active.pending_ultrathink = false;
        }
        // ACP agents apply every option change live; pi applies its thinking
        // level live. Route those choices back instead of waiting for a restart.
        if active.meta.provider.caps().live_option_push.supports(id)
            && let Runtime::Live(commands) = &active.runtime
            && let Some(selection) = active.meta.option_selections.iter().find(|s| s.id == id)
        {
            let _ = commands.try_send(SessionCommand::SetOption {
                id: selection.id.clone(),
                value: selection.value.clone(),
            });
            active.live_option_selections = active.meta.option_selections.clone();
        }
        self.preview_draft_or_persist_active(cx);
    }

    /// Arm an Ultrathink turn: the next send is prefixed with `Ultrathink:\n`.
    /// T3 does not persist this as an option (it resolves back to the default),
    /// so it lives as a transient per-send flag.
    pub fn select_ultrathink(&mut self, _cx: &mut HostCx) {
        if let Some(active) = self.residents.active.as_mut() {
            active.pending_ultrathink = true;
        }
    }

    // -- interaction mode (Build / Plan) ------------------------------------

    /// The active session's Build/Plan interaction mode (`Build` when none).
    pub(crate) fn active_interaction_mode(&self) -> InteractionMode {
        self.residents
            .active
            .as_ref()
            .map(|a| a.meta.interaction_mode)
            .unwrap_or_default()
    }

    /// Set the Build/Plan interaction mode for the active session. Both
    /// providers switch live (Codex per turn, Claude via a control request), so
    /// no restart is scheduled.
    pub fn set_interaction_mode(&mut self, mode: InteractionMode, cx: &mut HostCx) {
        let Some(active) = self.residents.active.as_mut() else {
            return;
        };
        if active.meta.interaction_mode == mode {
            return;
        }
        active.meta.interaction_mode = mode;
        if let Runtime::Live(commands) = &active.runtime {
            let _ = commands.try_send(SessionCommand::SetInteractionMode(mode));
        }
        self.preview_draft_or_persist_active(cx);
    }

    /// Toggle Build ↔ Plan (the chip click and Shift+Tab).
    pub fn toggle_interaction_mode(&mut self, cx: &mut HostCx) {
        let next = match self.active_interaction_mode() {
            InteractionMode::Build => InteractionMode::Plan,
            InteractionMode::Plan => InteractionMode::Build,
        };
        self.set_interaction_mode(next, cx);
    }

    // -- proposed-plan flow -------------------------------------------------

    /// Accept the proposed plan: send the verbatim implementation prompt, switch
    /// to Build mode, and persist the decision before dispatching the turn.
    pub fn implement_plan(&mut self, cx: &mut HostCx) {
        // A pending provider handoff makes `send_turn` defer. Validate it before
        // resolving the plan or changing mode, or the implementation prompt can
        // vanish while the UI claims the plan was accepted.
        if self.relay_confirmation().is_some() {
            return;
        }
        let Some((session_id, item_id, markdown)) =
            self.residents.active.as_ref().and_then(|active| {
                active.timeline.plan_ready().map(|plan| {
                    (
                        active.meta.id.clone(),
                        plan.item_id.clone(),
                        plan.markdown.clone(),
                    )
                })
            })
        else {
            return;
        };
        self.record_event(
            &session_id,
            &AgentEvent::PlanResolved {
                item_id,
                resolution: PlanResolution::Implemented,
            },
            cx,
        );
        self.set_interaction_mode(InteractionMode::Build, cx);
        self.send_turn_assembled(implement_prompt(&markdown), Vec::new(), cx);
    }

    /// Leave the plan captured in history while removing its actionable
    /// composer state.
    pub fn dismiss_plan(&mut self, cx: &mut HostCx) {
        let Some((session_id, item_id)) = self.residents.active.as_ref().and_then(|active| {
            active
                .timeline
                .plan_ready()
                .map(|plan| (active.meta.id.clone(), plan.item_id.clone()))
        }) else {
            return;
        };
        self.record_event(
            &session_id,
            &AgentEvent::PlanResolved {
                item_id,
                resolution: PlanResolution::Dismissed,
            },
            cx,
        );
    }

    /// Accept the proposed plan in a fresh thread in the same project (same
    /// cwd/model/options, Build mode) titled "Implement <plan title>".
    pub fn implement_plan_in_new_thread(&mut self, title: String, cx: &mut HostCx) {
        let Some(active) = self.residents.active.as_ref() else {
            return;
        };
        let Some(plan) = active.timeline.plan_ready() else {
            return;
        };
        let source_session_id = active.meta.id.clone();
        let plan_item_id = plan.item_id.clone();
        let markdown = plan.markdown.clone();
        let provider = active.meta.provider;
        let cwd = active.meta.cwd.clone();
        let model = active.meta.model.clone();
        let option_selections = active.meta.option_selections.clone();
        let approval_mode = active.meta.approval_mode;
        let project_id = active.meta.project_id.clone();
        let acp_agent_id = active.meta.acp_agent_id.clone();
        let profile_id = active.meta.profile_id.clone();

        let mut meta = SessionMeta::new(provider, cwd, model);
        meta.title = title;
        meta.option_selections = option_selections;
        meta.approval_mode = approval_mode;
        meta.interaction_mode = InteractionMode::Build;
        meta.project_id = project_id;
        meta.acp_agent_id = acp_agent_id;
        meta.profile_id = profile_id;
        let destination_session_id = meta.id.clone();
        self.record_event(
            &source_session_id,
            &AgentEvent::PlanResolved {
                item_id: plan_item_id,
                resolution: PlanResolution::HandedOff {
                    session_id: destination_session_id,
                },
            },
            cx,
        );
        self.enqueue_store_write(
            StoreWrite::UpsertMeta {
                meta: Box::new(meta.clone()),
                initial: true,
            },
            cx,
        );
        self.upsert_session_in_memory(meta.clone());
        self.park_active(cx);
        let session_id = meta.id.clone();
        let cwd = meta.cwd.clone();
        let provider_commands =
            self.cached_provider_commands(meta.provider, meta.acp_agent_id.as_deref());
        self.residents.active = Some(ActiveSession::new(meta, false, provider_commands));
        self.emit_domain(
            Topic::SessionEvents {
                session_id: session_id.clone(),
            },
            ServerEvent::SessionSnapshot(Vec::new()),
            cx,
        );
        self.refresh_session_git_branch(session_id, cwd, cx);
        self.send_turn_assembled(implement_prompt(&markdown), Vec::new(), cx);
    }

    /// Copy plan markdown to the clipboard (the "Copy to clipboard" action).
    pub fn copy_plan(&mut self, markdown: String, cx: &mut HostCx) {
        emit_runtime(
            cx,
            RuntimeEvent::Effect(RuntimeEffect::CopyToClipboard { text: markdown }),
        );
    }

    /// Write the plan markdown to `PLAN-<n>.md` in the session cwd, choosing the
    /// lowest unused index ("Save to workspace"). Emits a success/error notice.
    pub fn save_plan_to_workspace(&mut self, markdown: String, cx: &mut HostCx) {
        let Some(cwd) = self.residents.active.as_ref().map(|a| a.meta.cwd.clone()) else {
            return;
        };
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || user_files::save_plan_to_workspace(&cwd, &markdown))
                .await;
            host_cx.enqueue(move |state, cx| state.finish_plan_save(result, cx));
        });
    }

    /// Save the plan markdown to the user's Downloads directory (falling back to
    /// the session cwd) with a title-derived filename ("Download as markdown").
    pub fn download_plan(&mut self, markdown: String, fallback_title: String, cx: &mut HostCx) {
        let title = plan_title(&markdown).unwrap_or(fallback_title);
        let filename = format!("{}.md", sanitize_filename(&title));
        let fallback_cwd = self.residents.active.as_ref().map(|a| a.meta.cwd.clone());
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || {
                    user_files::save_plan_download(&filename, &markdown, fallback_cwd.as_deref())
                })
                .await;
            host_cx.enqueue(move |state, cx| state.finish_plan_save(result, cx));
        });
    }

    pub(super) fn finish_plan_save(&mut self, result: std::io::Result<PathBuf>, cx: &mut HostCx) {
        match result {
            Ok(path) => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                emit_runtime(
                    cx,
                    RuntimeEvent::Notice(RuntimeNotice::PlanSaved { file: name }),
                );
            }
            Err(error) => self.report_error(
                RuntimeError::PersistEvent {
                    error: error.to_string(),
                },
                cx,
            ),
        }
    }

    // -- git branch picker (checkout row) -----------------------------------

    /// Load the local branches for the active session's cwd in the background
    /// (called when the checkout-row popover opens).
    pub fn load_branches(&mut self, cx: &mut HostCx) {
        let Some(active) = self.residents.active.as_ref() else {
            return;
        };
        let cwd = active.meta.cwd.clone();
        let session_id = active.meta.id.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let branches = host_cx.unblock(move || list_git_branches(&cwd)).await;
            host_cx.enqueue(move |state, _cx| {
                if let Some(active) = state.residents.active.as_mut()
                    && active.meta.id == session_id
                {
                    active.branches = branches;
                }
            });
        });
    }

    /// Check out `branch` in the active session's cwd, if the working tree is
    /// clean. Runs git off the main thread; reports success/failure as an
    /// `RuntimeEvent` the chat view turns into a notification.
    pub fn checkout_branch(&mut self, branch: String, cx: &mut HostCx) {
        let Some(active) = self.residents.active.as_ref() else {
            return;
        };
        if active.timeline.turn_running {
            return;
        }
        let cwd = active.meta.cwd.clone();
        let session_id = active.meta.id.clone();
        let branch_for_task = branch.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || checkout_if_clean(&cwd, &branch_for_task))
                .await;
            host_cx.enqueue(move |state, cx| match result {
                Ok(()) => {
                    if let Some(cwd) = state
                        .residents
                        .active
                        .as_ref()
                        .filter(|active| active.meta.id == session_id)
                        .map(|active| active.meta.cwd.clone())
                    {
                        state.refresh_session_git_branch(session_id.clone(), cwd, cx);
                    }
                    emit_runtime(
                        cx,
                        RuntimeEvent::Notice(RuntimeNotice::SwitchedBranch { branch }),
                    );
                }
                Err(CheckoutError::Dirty) => {
                    emit_runtime(cx, RuntimeEvent::Error(RuntimeError::DirtyTree));
                }
                Err(CheckoutError::Git(message)) => {
                    emit_runtime(cx, RuntimeEvent::Error(RuntimeError::External(message)))
                }
            });
        });
    }

    /// Select `mode` for the active session and persist it. Claude applies the
    /// switch live over the control protocol; Codex (which binds the mode at
    /// thread start) instead restarts via the resume cursor on the next turn.
    pub fn set_active_approval_mode(&mut self, mode: ApprovalMode, cx: &mut HostCx) {
        let Some(active) = self.residents.active.as_mut() else {
            return;
        };
        if active.meta.approval_mode == mode {
            return;
        }
        active.meta.approval_mode = mode;
        active.meta.updated_at = now_secs();

        if let Runtime::Live(commands) = &active.runtime {
            let _ = commands.try_send(SessionCommand::SetApprovalMode(mode));
            // Claude applies the switch live: keep `live_approval_mode` in sync so
            // no restart is scheduled. Codex can't, so leave it stale — the next
            // `send_turn` sees the mismatch and restarts from the resume cursor.
            if active.meta.provider.caps().live_approval_mode_switch {
                active.live_approval_mode = Some(mode);
            }
        }

        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
    }

    /// Toggle a model id in the persisted favorites list.
    pub fn toggle_favorite_model(&mut self, model: &str, cx: &mut HostCx) {
        let mut settings = self.settings.clone();
        if let Some(pos) = settings.favorite_models.iter().position(|m| m == model) {
            settings.favorite_models.remove(pos);
        } else {
            settings.favorite_models.push(model.to_string());
        }
        self.update_settings(settings, cx);
    }
}

/// A filesystem-safe filename fragment: replace path separators and control
/// characters with `-`, collapse runs, and cap the length.
pub(super) fn sanitize_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '-'
            } else {
                c
            }
        })
        .collect();
    out = out.trim().trim_matches('-').to_string();
    if out.is_empty() {
        out = "plan".to_string();
    }
    out.chars().take(80).collect()
}
