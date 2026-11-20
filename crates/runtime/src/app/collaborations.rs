//! Collaborations as the host runs them: the thread a collaboration is chosen for, the brief its decision
//! model is given, and the thread that drafts a mandate.

use super::*;
use tcode_core::collaboration::{self, ModelRef};

impl AppState {
    /// Run thread `target_id` under collaboration `collaboration_id`, on its decision model.
    pub fn set_active_collaboration(
        &mut self,
        target_id: &str,
        collaboration_id: &str,
        cx: &mut HostCx,
    ) {
        let Some(lead) = self
            .settings
            .collaboration(collaboration_id)
            .map(|collaboration| collaboration.lead.clone())
        else {
            return;
        };
        self.set_active_model(
            target_id,
            lead.provider,
            Some(lead.model),
            lead.profile_id,
            cx,
        );
        self.set_collaboration(target_id, Some(collaboration_id.to_owned()), cx);
    }

    /// Put the thread under a collaboration, or with `None` out from under the one it had.
    pub(crate) fn set_collaboration(
        &mut self,
        target_id: &str,
        collaboration_id: Option<String>,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        if active.meta.collaboration_id == collaboration_id {
            return;
        }
        active.meta.collaboration_id = collaboration_id;
        let relayed = active.pending_relay.is_some();
        self.refresh_collaboration_briefs();
        // A thread waiting on a relay keeps its stored setup until the relay is confirmed.
        if !relayed {
            self.preview_draft_or_persist_active(target_id, cx);
        }
    }

    /// What the thread's model is told of the collaboration it runs under: nothing where the
    /// provider has no channel for it, or is not given `tcode_control` to start a
    /// collaborator with.
    pub(super) fn collaboration_brief(&self, meta: &SessionMeta) -> Option<String> {
        let collaboration = self
            .settings
            .collaboration(meta.collaboration_id.as_deref()?)?;
        if !self.control_offered(meta)
            || meta.provider.caps().instruction_channel == agent::InstructionChannel::None
        {
            return None;
        }
        Some(collaboration::brief(
            collaboration,
            &self.collaboration_templates.lead,
            |model| self.model_names(model),
            |model| {
                self.settings
                    .model_guide(model)
                    .map(|guide| guide.text.clone())
            },
        ))
    }

    /// A model's name and its account's, as the user reads them.
    fn model_names(&self, model: &ModelRef) -> (String, String) {
        let name = self
            .models_for(model.provider)
            .iter()
            .find(|spec| spec.id == model.model)
            .map_or_else(|| model.model.clone(), |spec| spec.display_name.clone());
        (name, self.profile_display_name(model.profile()))
    }

    /// Every resident thread's brief as its collaboration and the settings now give it; a live
    /// provider told another one restarts before its next turn.
    pub(super) fn refresh_collaboration_briefs(&mut self) {
        let briefs: Vec<(String, Option<String>)> = self
            .residents
            .live
            .values()
            .chain(self.residents.parked.values())
            .map(|session| {
                (
                    session.meta.id.clone(),
                    self.collaboration_brief(&session.meta),
                )
            })
            .collect();
        for (id, brief) in briefs {
            if let Some(session) = self.resident_mut(&id) {
                session.collaboration_brief = brief;
            }
        }
    }

    /// Whether `tcode_control` may start a thread on this account and model
    /// (the provider's default model when `None`), by Settings → Control.
    pub(super) fn control_agents_allow(
        &self,
        provider: ProviderKind,
        profile_id: Option<&str>,
        acp_agent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), String> {
        let profile = profile_id.unwrap_or_else(|| Settings::builtin_profile_id(provider));
        let scope = &self.settings.control.agents;
        let refused = |what: String| {
            Err(format!(
                "refused: Settings → Control does not let tcode_control start threads on {what}; \
                 list_providers lists what it may"
            ))
        };
        let Some(access) = scope.agent(profile) else {
            return refused(self.profile_display_name(profile));
        };
        if provider == ProviderKind::Acp {
            return if access.allows_acp_agent(acp_agent_id) {
                Ok(())
            } else {
                refused(format!(
                    "the ACP agent {}",
                    acp_agent_id.unwrap_or_default()
                ))
            };
        }
        let model = model.map(str::to_owned).or_else(|| {
            self.models_for(provider)
                .iter()
                .find(|spec| spec.is_default)
                .map(|spec| spec.id.clone())
        });
        if access.allows_model(model.as_deref()) {
            Ok(())
        } else {
            refused(format!(
                "the model {} of {}",
                model.as_deref().unwrap_or("the profile's default"),
                self.profile_display_name(profile)
            ))
        }
    }

    /// Open the quick thread's new-thread page on `lead`, a collaboration's decision model,
    /// as the thread that researches `model`'s guide: it alone is given the tool that
    /// writes the guide. Nothing is sent; the user sends the request themselves.
    pub fn research_model(
        &self,
        model: ModelRef,
        lead: ModelRef,
        cx: &HostCx,
    ) -> HostTask<Result<String, ProtocolError>> {
        let given = lead.provider.caps().mcp_servers && self.settings.control.enabled;
        let draft = self.start_scratch_draft(cx);
        let host = cx.clone();
        cx.spawn_background(async move {
            if !given {
                return Err(ProtocolError {
                    code: "collaboration_needs_control".into(),
                    message: "The decision model writes a guide through tcode_control, which this thread would not be given.".into(),
                });
            }
            let id = draft.await?;
            let opened = id.clone();
            host.enqueue_and_wait(move |app, cx| {
                app.set_collaboration(&opened, None, cx);
                app.set_active_model(
                    &opened,
                    lead.provider,
                    Some(lead.model),
                    lead.profile_id,
                    cx,
                );
                if let Some(draft) = app.resident_mut(&opened) {
                    draft.meta.guide_for = Some(model);
                }
            })
            .await
            .map_err(|()| ProtocolError {
                code: "transport_closed".into(),
                message: "Host stopped before opening the thread.".into(),
            })?;
            Ok(id)
        })
    }

    pub fn collaboration_templates(&self) -> collaboration::Templates {
        self.collaboration_templates.clone()
    }

    /// Fetch the templates the repository publishes and take them where they are newer
    /// than the ones in effect, keeping the file for the next start.
    pub fn refresh_collaboration_templates(
        &mut self,
        cx: &mut HostCx,
    ) -> HostTask<Result<(), ProtocolError>> {
        let fetched = cx.unblock(tcode_services::collaboration_templates::fetch);
        let cache = tcode_services::collaboration_templates::cache_path(self.store.root());
        let host_cx = cx.clone();
        cx.spawn_background(async move {
            let unfetched = |message: String| ProtocolError {
                code: "templates_unfetched".into(),
                message,
            };
            let (text, templates) = fetched.await.map_err(unfetched)?;
            host_cx
                .enqueue_and_wait(move |state, cx| {
                    if templates.updated_at <= state.collaboration_templates.updated_at {
                        return Ok(());
                    }
                    state.collaboration_templates = templates;
                    state.refresh_collaboration_briefs();
                    let written = cx.unblock(move || std::fs::write(&cache, text));
                    HostCx::spawn_detached(cx, async move {
                        if let Err(error) = written.await {
                            log::warn!("could not keep the collaboration templates: {error}");
                        }
                    });
                    Ok(())
                })
                .await
                .map_err(|()| unfetched("The host stopped before the templates arrived.".into()))?
        })
    }
}
