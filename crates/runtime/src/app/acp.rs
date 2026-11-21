use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

/// A registry agent being installed. Setting `cancelled` stops its download.
pub(crate) struct AcpInstall {
    pub(super) operation: RuntimeOperationId,
    pub(super) name: String,
    pub(super) cancelled: Arc<AtomicBool>,
}

impl AppState {
    /// Load the registry index (cache first, network when stale). Cheap enough
    /// to call every time the Providers page opens.
    pub fn refresh_acp_registry(&mut self, cx: &mut HostCx) {
        if self.acp_registry_loading {
            return;
        }
        self.acp_registry_loading = true;
        let data_dir = self.store.root().clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let cache_dir = data_dir.clone();
            let cached_registry = host_cx.unblock(move || cached(&cache_dir)).await;
            host_cx.enqueue(move |state, _cx| {
                if state.acp_registry_loading && state.acp_registry.is_none() {
                    *state.acp_registry = cached_registry;
                }
            });
            let result = host_cx.unblock(move || load(&data_dir)).await;
            host_cx.enqueue(move |state, _cx| {
                state.acp_registry_loading = false;
                match result {
                    Ok(registry) => {
                        *state.acp_registry = Some(registry);
                        state.acp_registry_error = None;
                    }
                    Err(err) => {
                        log::warn!("ACP registry unavailable: {err}");
                        state.acp_registry_error = Some(err.to_string());
                    }
                }
            });
        });
    }

    /// The marketplace list: every registry agent except the hidden adapters
    /// over our own native CLIs.
    pub(crate) fn acp_marketplace(&self) -> Vec<RegistryAgent> {
        self.acp_registry
            .as_ref()
            .map(|registry| visible_agents(registry).into_iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Runtime-owned marketplace views in the registry's visible ordering.
    pub(crate) fn acp_marketplace_items(&self) -> Vec<AcpMarketplaceItem> {
        let platform = platform_key();
        self.acp_registry
            .as_ref()
            .map(|registry| {
                visible_agents(registry)
                    .into_iter()
                    .map(|agent| AcpMarketplaceItem {
                        id: agent.id.clone(),
                        name: agent.name.clone(),
                        version: agent.version.clone(),
                        description: agent.description.clone(),
                        installed: self.settings.acp_agents.contains_key(&agent.id),
                        installing: self.acp_installing.contains_key(&agent.id),
                        supported: resolve_recipe(agent, &platform).is_some(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every harness as clients present it: the native ones, then each
    /// installed ACP agent with the registry's newer version, if any.
    pub(crate) fn harness_descriptions(&self) -> Vec<tcode_protocol::HarnessDescription> {
        let registry = self.acp_marketplace();
        let native = ProviderKind::NATIVE
            .into_iter()
            .map(tcode_protocol::HarnessDescription::native);
        let agents = self.settings.acp_agents.values().map(|agent| {
            let caps = ProviderKind::Acp.caps();
            let newer = registry
                .iter()
                .find(|entry| entry.id == agent.id)
                .map(|entry| entry.version.clone())
                .filter(|latest| {
                    provider_updates::parse_version(latest)
                        .zip(provider_updates::parse_version(&agent.version))
                        .is_some_and(|(latest, installed)| latest > installed)
                });
            tcode_protocol::HarnessDescription {
                acp_agent_id: Some(agent.id.clone()),
                launch_summary: Some(launch_summary(&agent.launch)),
                launch_args: caps.launch_args,
                mcp_servers: caps.mcp_servers,
                registry_update: newer,
                ..tcode_protocol::HarnessDescription::native(ProviderKind::Acp)
            }
        });
        native.chain(agents).collect()
    }

    pub(super) fn acp_installing_ids(&self) -> HashSet<String> {
        self.acp_installing.keys().cloned().collect()
    }

    /// Download + install one registry agent, with a progress toast the user
    /// can cancel it from.
    pub fn install_acp_agent(&mut self, id: String, cx: &mut HostCx) {
        let Some(agent) = self
            .acp_marketplace()
            .into_iter()
            .find(|agent| agent.id == id)
        else {
            return;
        };
        if self.acp_installing.contains_key(&id) {
            return;
        }
        let operation = self.next_operation_id();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.acp_installing.insert(
            id.clone(),
            AcpInstall {
                operation,
                name: agent.name.clone(),
                cancelled: cancelled.clone(),
            },
        );
        let data_dir = self.store.root().clone();
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::AcpInstallStarted {
                operation,
                id: id.clone(),
                name: agent.name.clone(),
            }),
        );
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || install(&agent, &data_dir, &cancelled))
                .await;
            host_cx.enqueue(move |state, cx| {
                // A cancelled install was already reported, and the id may
                // be installing again since.
                if state
                    .acp_installing
                    .get(&id)
                    .is_none_or(|install| install.operation != operation)
                {
                    return;
                }
                let Some(install) = state.acp_installing.remove(&id) else {
                    return;
                };
                let name = install.name;
                match result {
                    Ok(mut installed) => {
                        // An update keeps what the user set for the agent.
                        if let Some(previous) = state.settings.acp_agents.get(&id) {
                            installed.enabled = previous.enabled;
                            installed.env = previous.env.clone();
                            installed.launch_args = previous.launch_args.clone();
                        }
                        state.settings.acp_agents.insert(id.clone(), installed);
                        state.persist_settings(cx);
                        state.refresh_acp_catalog(&id, cx);
                        emit_runtime(
                            cx,
                            RuntimeEvent::Toast(RuntimeToast::AcpInstallSucceeded {
                                operation,
                                name,
                            }),
                        );
                    }
                    Err(err) => emit_runtime(
                        cx,
                        RuntimeEvent::Toast(RuntimeToast::AcpInstallFailed {
                            operation,
                            name,
                            detail: err.to_string(),
                        }),
                    ),
                }
            });
        });
    }

    /// Stop an agent's install: its download ends at the next read and
    /// nothing is written.
    pub(crate) fn cancel_acp_install(&mut self, id: &str, cx: &mut HostCx) {
        let Some(install) = self.acp_installing.remove(id) else {
            return;
        };
        install.cancelled.store(true, Ordering::Relaxed);
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::AcpInstallCancelled {
                operation: install.operation,
                name: install.name,
            }),
        );
    }

    /// Remove an installed ACP agent (its files and its settings entry).
    pub fn remove_acp_agent(&mut self, id: &str, cx: &mut HostCx) {
        if self.settings_change_refused(cx) {
            return;
        }
        self.settings.acp_agents.remove(id);
        self.settings.favorite_models.retain(|favorite| {
            !matches!(
                favorite,
                FavoriteModel::Row {
                    acp_agent_id: Some(starred),
                    ..
                } if starred == id
            )
        });
        self.persist_settings(cx);
        self.providers.cancel_acp_refresh(id);
        self.providers.acp_catalogs.remove(id);
        let store = self.store.clone();
        let id = id.to_string();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let removed = host_cx
                .unblock({
                    let id = id.clone();
                    move || {
                        let uninstalled = uninstall(store.root(), &id);
                        store.remove_acp_catalog(&id).and(uninstalled)
                    }
                })
                .await;
            if let Err(err) = removed {
                host_cx.enqueue(move |state, cx| {
                    state.report_error(
                        RuntimeError::External(format!(
                            "could not remove the files of ACP agent {id}: {err}"
                        )),
                        cx,
                    )
                });
            }
        });
    }

    /// Register a user-defined ACP agent (the escape hatch for anything not in
    /// the registry): an arbitrary command that speaks ACP over its stdio.
    pub fn add_custom_acp_agent(
        &mut self,
        name: String,
        command: String,
        args: Vec<String>,
        cx: &mut HostCx,
    ) {
        let name = name.trim().to_string();
        let command = command.trim().to_string();
        if name.is_empty() || command.is_empty() || self.settings_change_refused(cx) {
            return;
        }
        let id = custom_acp_id(&name);
        self.settings.acp_agents.insert(
            id.clone(),
            InstalledAgent {
                id: id.clone(),
                name,
                version: String::new(),
                icon: None,
                launch: agent::AcpLaunch::Custom {
                    command,
                    args,
                    env: Vec::new(),
                },
                enabled: true,
                env: Vec::new(),
                launch_args: None,
            },
        );
        self.persist_settings(cx);
        self.refresh_acp_catalog(&id, cx);
    }

    /// Update one installed ACP agent in place (enable switch, env rows, args).
    /// An agent that now runs differently lists its catalog again.
    pub fn update_acp_agent(&mut self, id: &str, patch: AcpAgentPatch, cx: &mut HostCx) {
        if self.settings_change_refused(cx) {
            return;
        }
        let Some(agent) = self.settings.acp_agents.get_mut(id) else {
            return;
        };
        let before = agent.clone();
        match patch {
            AcpAgentPatch::SetEnabled { enabled } => agent.enabled = enabled,
            AcpAgentPatch::SetLaunchArgs { launch_args } => {
                agent.launch_args = launch_args
                    .map(|args| args.trim().to_owned())
                    .filter(|args| !args.is_empty());
            }
            AcpAgentPatch::SetEnv { env } => agent.env = named(env),
        }
        let changed = *agent != before;
        self.persist_settings(cx);
        if changed {
            self.refresh_acp_catalog(id, cx);
        }
    }

    /// List installed ACP agent `id`'s models and options
    /// ([`agent::acp::probe_catalog`]) in the background, in place of a
    /// listing still running. A disabled agent is not listed.
    pub fn refresh_acp_catalog(&mut self, id: &str, cx: &mut HostCx) {
        let Some(installed) = self
            .settings
            .acp_agents
            .get(id)
            .filter(|agent| agent.enabled && agent.offered_for_new_sessions())
            .cloned()
        else {
            self.providers.cancel_acp_refresh(id);
            return;
        };
        let extra_args = match installed.extra_args() {
            Ok(args) => args,
            Err(error) => {
                log::warn!("ACP agent {id} cannot list its catalog: launch arguments: {error}");
                self.providers.cancel_acp_refresh(id);
                return;
            }
        };
        let (generation, processes) = self.providers.begin_acp_refresh(id);
        let launch_env = self.prepare_acp_env(id, cx);
        let agent = agent::AcpAgent {
            id: installed.id.clone(),
            name: installed.name.clone(),
            launch: installed.launch.clone(),
        };
        let store = self.store.clone();
        let host_cx = cx.clone();
        let id = id.to_owned();
        HostCx::spawn_detached(cx, async move {
            let result = match launch_env.await {
                Ok(launch_env) => {
                    agent::acp::probe_catalog(agent, extra_args, launch_env, &processes)
                        .await
                        .map_err(|error| error.to_string())
                }
                Err(error) => Err(error),
            };
            host_cx.enqueue(move |state, _cx| {
                if !state.providers.finish_acp_refresh(&id, generation)
                    || state.settings.acp_agents.get(&id) != Some(&installed)
                {
                    return;
                }
                match result {
                    Ok(catalog) => {
                        if let Err(error) = store.save_acp_catalog(&id, &catalog) {
                            log::warn!("could not save the catalog of ACP agent {id}: {error}");
                        }
                        state.providers.acp_catalogs.insert(id.clone(), catalog);
                        state.seed_acp_drafts(&id);
                    }
                    Err(error) => {
                        log::warn!("could not list the catalog of ACP agent {id}: {error}")
                    }
                }
            });
        });
    }

    /// List the catalog of every enabled ACP agent that has none yet, as the
    /// host starts.
    pub(crate) fn refresh_missing_acp_catalogs(&mut self, cx: &mut HostCx) {
        let missing: Vec<String> = self
            .settings
            .acp_agents
            .keys()
            .filter(|id| !self.providers.acp_catalogs.contains_key(*id))
            .cloned()
            .collect();
        for id in missing {
            self.refresh_acp_catalog(&id, cx);
        }
    }

    /// The model a thread on ACP agent `id` runs when it chose none.
    pub(super) fn acp_default_model(&self, id: &str) -> Option<&str> {
        self.providers
            .acp_catalogs
            .get(id)
            .and_then(agent::AcpCatalog::default_model)
    }

    /// Give every unsent draft on ACP agent `id` what the agent's catalog
    /// offers ([`seed_from_catalog`]).
    fn seed_acp_drafts(&mut self, id: &str) {
        let Some(catalog) = self.providers.acp_catalogs.get(id).cloned() else {
            return;
        };
        for active in self
            .residents
            .live
            .values_mut()
            .chain(self.residents.parked.values_mut())
            .filter(|active| active.draft && active.meta.acp_agent_id.as_deref() == Some(id))
        {
            seed_from_catalog(active, &catalog);
        }
    }

    /// [`seed_from_catalog`] for `active` with its ACP agent's catalog, if
    /// it runs one that has one.
    pub(super) fn seed_acp_session(&self, active: &mut ActiveSession) {
        if let Some(catalog) = active
            .meta
            .acp_agent_id
            .as_deref()
            .and_then(|id| self.providers.acp_catalogs.get(id))
        {
            seed_from_catalog(active, catalog);
        }
    }

    pub(super) fn preview_draft_or_persist_active(&mut self, target_id: &str, cx: &mut HostCx) {
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        if active.draft {
            return;
        }
        active.meta.updated_at = cx.clock().unix_secs();
        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
    }

    /// Run the thread on installed ACP agent `id` (a row of the model
    /// picker's rail for that agent): on `model`, one of the agent's own, or
    /// with none on the one it starts with. Another model of the agent the
    /// thread runs is taken where the agent runs, keeping the thread's
    /// options. Another agent cannot take up this one's session, so a thread
    /// with history relays it once confirmed, as for another provider.
    pub fn set_active_acp_agent(
        &mut self,
        target_id: &str,
        id: &str,
        model: Option<String>,
        cx: &mut HostCx,
    ) {
        let provider_commands = self.cached_provider_commands(ProviderKind::Acp, None, Some(id));
        let catalog = self.providers.acp_catalogs.get(id).cloned();
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        if active.meta.provider == ProviderKind::Acp
            && active.meta.acp_agent_id.as_deref() == Some(id)
        {
            if model.is_none() || active.meta.model == model {
                return;
            }
            active.meta.model = model;
            if active.pending_relay.is_none() {
                self.preview_draft_or_persist_active(target_id, cx);
            }
            return;
        }
        let mut model = model;
        let mut option_selections = Vec::new();
        let mut abandoned = false;
        if !active.draft {
            let source = active.pending_relay.clone().unwrap_or(PendingRelay {
                from_provider: active.meta.provider,
                from_model: active.meta.model.clone(),
                from_profile: active.meta.profile_id.clone(),
                from_acp_agent: active.meta.acp_agent_id.clone(),
                from_options: active.meta.option_selections.clone(),
            });
            if active.pending_relay.is_some()
                && source.from_provider == ProviderKind::Acp
                && source.from_acp_agent.as_deref() == Some(id)
            {
                // Back on the agent that holds the session, as it ran.
                active.pending_relay = None;
                model = model.or(source.from_model);
                option_selections = source.from_options;
            } else if has_meaningful_history(&active.timeline) {
                active.pending_relay = Some(source);
            } else {
                active.resume_cursor_for_fresh_provider();
                abandoned = true;
            }
        }
        active.meta.provider = ProviderKind::Acp;
        active.meta.acp_agent_id = Some(id.to_string());
        active.meta.profile_id = None;
        active.meta.model = model;
        active.meta.option_selections = option_selections;
        active.provider_options.clear();
        active.confirmed_option_selections.clear();
        active.provider_commands = provider_commands;
        if let Some(catalog) = &catalog {
            seed_from_catalog(active, catalog);
        }
        if active.pending_relay.is_some() {
            return;
        }
        if abandoned {
            self.close_abandoned_turn(target_id, cx);
        }
        self.preview_draft_or_persist_active(target_id, cx);
    }

    /// Reset the *preferences* to defaults. Anything the user had to configure
    /// or install survives: provider profiles and their credentials, the ACP
    /// agents on disk, and the legacy binary overrides — wiping those would cost
    /// re-authentication and re-downloads, which is never what "restore
    /// defaults" means. Sidebar/UI state (collapsed projects, favorites, sort,
    /// layout, last-visited) is preserved for the same reason, and `unknown`
    /// must survive so a newer build's keys are not destroyed by an older one.
    /// The theme is reset; the caller re-applies it to the window.
    pub fn reset_settings(&mut self, cx: &mut HostCx) {
        let settings = Settings {
            providers: self.settings.providers.clone(),
            profiles: self.settings.profiles.clone(),
            codex_binary: self.settings.codex_binary.clone(),
            claude_binary: self.settings.claude_binary.clone(),
            acp_agents: self.settings.acp_agents.clone(),
            sidebar_collapsed: self.settings.sidebar_collapsed,
            collapsed_projects: self.settings.collapsed_projects.clone(),
            favorite_models: self.settings.favorite_models.clone(),
            project_sort: self.settings.project_sort,
            sidebar_layout: self.settings.sidebar_layout,
            last_visited: self.settings.last_visited.clone(),
            unknown: self.settings.unknown.clone(),
            ..Settings::default()
        };
        self.update_settings(settings, cx);
    }
}

/// An ACP thread stored while its agent's model was one of its options holds
/// the model it last ran or chose as that option's selection, and the model
/// its first session reported as its model: the selection is its model.
pub(super) fn adopt_stored_acp_model(meta: &mut SessionMeta) {
    if meta.provider != ProviderKind::Acp {
        return;
    }
    let Some(index) = meta
        .option_selections
        .iter()
        .position(|selection| selection.id == agent::ACP_MODEL_OPTION_ID)
    else {
        return;
    };
    let selection = meta.option_selections.remove(index);
    if let Some(model) = selection.value.as_str() {
        meta.model = Some(model.to_owned());
    }
}

/// What an ACP thread shows before its agent runs, from what the agent
/// offers a new session: the model it starts on, unless the thread chose one,
/// and its options, each at the value a session starts at unless the thread
/// chose another. The options are the catalog's until the agent's own
/// process describes them.
fn seed_from_catalog(active: &mut ActiveSession, catalog: &agent::AcpCatalog) {
    if active.meta.model.is_none() {
        active.meta.model = catalog.default_model().map(str::to_owned);
    }
    active.provider_options = catalog.options.clone();
}

/// A stable settings key for a user-defined ACP agent, derived from its name.
pub(super) fn custom_acp_id(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("custom:{}", slug.trim_matches('-'))
}

/// How an ACP agent is started, as one command line.
fn launch_summary(launch: &agent::AcpLaunch) -> String {
    let (command, args) = match launch {
        agent::AcpLaunch::Npx { package, args, .. } => (format!("npx {package}"), args),
        agent::AcpLaunch::Binary { command, args, .. } => (command.display().to_string(), args),
        agent::AcpLaunch::Custom { command, args, .. } => (command.clone(), args),
    };
    std::iter::once(command)
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}
