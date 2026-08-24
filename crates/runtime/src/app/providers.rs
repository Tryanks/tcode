use super::*;

pub struct ProviderCatalog {
    pub model_catalogs: HashMap<ProviderKind, Vec<ModelSpec>>,
    pub models_loading: HashMap<ProviderKind, bool>,
    pub provider_versions: HashMap<ProviderKind, ProviderVersionState>,
    pub tcode_update: TcodeUpdateState,
    pub provider_snapshots: HashMap<String, ProviderSnapshot>,
    pub(super) provider_secret_names: HashMap<String, HashSet<String>>,
}

impl ProviderCatalog {
    pub(super) fn new(
        model_catalogs: HashMap<ProviderKind, Vec<ModelSpec>>,
        provider_secret_names: HashMap<String, HashSet<String>>,
    ) -> Self {
        Self {
            model_catalogs,
            models_loading: HashMap::new(),
            provider_versions: HashMap::new(),
            tcode_update: TcodeUpdateState::default(),
            provider_snapshots: HashMap::new(),
            provider_secret_names,
        }
    }

    pub(super) fn status_snapshot(
        &self,
        acp_marketplace_items: Vec<AcpMarketplaceItem>,
        acp_registry_loading: bool,
        acp_registry_error: Option<String>,
        acp_installing: HashSet<String>,
    ) -> ProvidersStatus {
        ProvidersStatus {
            model_catalogs: self.model_catalogs.clone(),
            models_loading: self.models_loading.clone(),
            provider_versions: self
                .provider_versions
                .iter()
                .map(|(&provider, status)| {
                    (
                        provider,
                        ProtocolProviderVersionStatus {
                            installed: status.installed.clone(),
                            latest: status.latest.clone(),
                            update_available: status.update_available,
                            checking: status.checking,
                            updating: status.updating,
                            update_command: update_command_string(provider, status.install_source),
                        },
                    )
                })
                .collect(),
            tcode_update: TcodeUpdateStatus {
                current: self.tcode_update.current.clone(),
                latest: self.tcode_update.latest.clone(),
                release_url: self.tcode_update.release_url.clone(),
                update_available: self.tcode_update.update_available,
                checking: self.tcode_update.checking,
            },
            provider_snapshots: self.provider_snapshots.clone(),
            acp_marketplace_items,
            acp_registry_loading,
            acp_registry_error,
            acp_installing,
            providers_checked_at: self
                .provider_snapshots
                .values()
                .filter_map(|snapshot| snapshot.checked_at)
                .max(),
            providers_checking: self
                .provider_snapshots
                .values()
                .any(|snapshot| snapshot.checking)
                || self
                    .provider_versions
                    .values()
                    .any(|status| status.checking)
                || self.tcode_update.checking,
            secret_names: self.provider_secret_names.clone(),
        }
    }
}

impl AppState {
    /// Kick off a background refresh of every provider's model catalog (called
    /// at app start and after a binary-path change). Results update
    /// `model_catalogs` and are persisted so the next launch is instant.
    pub fn refresh_model_catalogs(&mut self, cx: &mut HostCx) {
        for provider in NATIVE_PROVIDER_KINDS {
            let binary = self.settings.provider(provider).binary_path;
            let settings = self.settings.clone();
            let settings_store = self.settings_store.clone();
            self.providers.models_loading.insert(provider, true);
            let store = self.store.clone();
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let profile_id = Settings::builtin_profile_id(provider).to_string();
                let launch_env = host_cx
                    .unblock(move || {
                        let secrets = settings_store.profile_secrets(&profile_id);
                        launch_env_for_profile(&settings, &profile_id, secrets)
                    })
                    .await;
                let result = list_models(provider, binary, launch_env).await;
                host_cx.enqueue(move |state, _cx| {
                    state.providers.models_loading.insert(provider, false);
                    match result {
                        Ok(models) if !models.is_empty() => {
                            if let Err(err) = store.save_models(provider, &models) {
                                log::warn!("failed to persist {provider:?} model catalog: {err}");
                            }
                            state.providers.model_catalogs.insert(provider, models);
                        }
                        Ok(_) => log::info!("{provider:?} returned an empty model catalog"),
                        Err(err) => log::warn!("failed to list {provider:?} models: {err}"),
                    }
                });
            });
        }
    }

    // -- provider version checks (Group C / s3 §6) --------------------------

    /// Whether the on-launch provider version check is enabled (default on).
    pub fn provider_update_checks_enabled(&self) -> bool {
        !self.settings.provider_update_checks_disabled
    }

    /// Resolve the binary path for a built-in provider profile.
    pub(super) fn resolve_provider_binary(&self, provider: ProviderKind) -> Option<PathBuf> {
        self.resolve_profile_binary(Settings::builtin_profile_id(provider))
    }

    /// Resolve the binary path for a profile: its settings override, else a
    /// PATH lookup of the protocol's bare command name.
    pub(super) fn resolve_profile_binary(&self, profile_id: &str) -> Option<PathBuf> {
        let profile = self.settings.resolved_profile(profile_id)?;
        profile
            .settings
            .binary_path
            .or_else(|| agent::find_on_path(&default_program(profile.kind)))
    }

    /// Re-run everything that depends on *how* a provider's CLI is launched
    /// (binary path, home, environment): its model catalog and its status probe.
    pub fn reload_provider(&mut self, cx: &mut HostCx) {
        self.refresh_model_catalogs(cx);
        self.refresh_provider_status(cx);
    }

    // -- provider profiles (built-in + user-created) ------------------------
    //
    // A *profile* is a named configuration on top of a protocol `ProviderKind`.
    // The built-in native-provider cards are profiles too (with stable ids such
    // as "claude", "codex", "pi", and "opencode").
    // The model catalog and update-check version stay keyed by kind; status
    // probes and card config (env, binary, home, accent, custom/hidden models)
    // are profile-specific, as are secrets.

    /// Every selectable native profile, grouped by kind. ACP is handled
    /// separately through the installed-agent list.
    pub(super) fn all_profiles(&self) -> Vec<ResolvedProfile> {
        NATIVE_PROVIDER_KINDS
            .iter()
            .flat_map(|kind| self.settings.profiles_for_kind(*kind))
            .collect()
    }

    /// Apply a serializable edit to one profile's card settings, routing built-in
    /// ids to their `providers` card and user ids to the `profiles` map.
    pub fn update_profile_settings(
        &mut self,
        id: &str,
        patch: ProfileSettingsPatch,
        cx: &mut HostCx,
    ) {
        let mut settings = self.settings.clone();
        let target = if let Some(kind) = Settings::builtin_kind_from_id(id) {
            settings.provider_mut(kind)
        } else if let Some(profile) = settings.profiles.get_mut(id) {
            &mut profile.settings
        } else {
            return;
        };
        match patch {
            ProfileSettingsPatch::SetEnabled { enabled } => target.enabled = enabled,
            ProfileSettingsPatch::ReplaceConfiguration(configuration) => {
                target.display_name = configuration.display_name;
                target.accent_color = configuration.accent_color;
                target.env = configuration.env;
                target.binary_path = configuration.binary_path;
                target.home_path = configuration.home_path;
                target.launch_args = configuration.launch_args;
                target.pi = configuration.pi;
                target.custom_models = configuration.custom_models;
                target.hidden_models = configuration.hidden_models;
            }
        }
        self.update_settings(settings, cx);
    }

    /// Store (or clear) one sensitive env value for a profile in `secrets.json`.
    pub fn set_profile_secret(
        &mut self,
        id: &str,
        name: &str,
        value: Option<&str>,
        cx: &mut HostCx,
    ) {
        self.enqueue_store_write(
            StoreWrite::SetProfileSecret {
                profile_id: id.to_string(),
                key: name.to_string(),
                value: value.map(str::to_string),
            },
            cx,
        );
        let names = self
            .providers
            .provider_secret_names
            .entry(id.to_string())
            .or_default();
        if value.is_some() {
            names.insert(name.to_string());
        } else {
            names.remove(name);
        }
    }

    /// Create a first-class *third-party* Claude Code profile from the Add-agent
    /// dialog: a named endpoint (Kimi preset or a custom Anthropic-compatible
    /// URL). Wires the three env vars, registers the model as a custom slug so it
    /// shows in the picker, gives the profile its own isolated `HOME` (seeded so
    /// `claude` runs non-interactively and never touches the official
    /// `~/.claude`), and stores the API key in `secrets.json`. Returns the id.
    pub fn create_third_party_profile(
        &mut self,
        name: &str,
        base_url: &str,
        model: Option<&str>,
        api_key: &str,
        cx: &mut HostCx,
    ) -> String {
        let name = name.trim();
        let name = if name.is_empty() { "Third-party" } else { name };
        let id = self.settings.allocate_profile_id(name);

        // Each third-party Claude profile gets an isolated HOME so its auth /
        // config never collides with the official Claude login. Seed onboarding
        // so the CLI starts straight into API-key mode.
        let home = self.store.root().join("profile-homes").join(&id);

        let mut env = vec![EnvVar {
            name: "ANTHROPIC_BASE_URL".into(),
            value: base_url.trim().to_string(),
            sensitive: false,
        }];
        if let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) {
            env.push(EnvVar {
                name: "ANTHROPIC_MODEL".into(),
                value: model.to_string(),
                sensitive: false,
            });
        }
        env.push(EnvVar {
            name: "ANTHROPIC_API_KEY".into(),
            value: String::new(),
            sensitive: true,
        });
        let custom_models = model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| vec![m.to_string()])
            .unwrap_or_default();

        let profile = ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                display_name: Some(name.to_string()),
                env,
                custom_models,
                home_path: Some(home.clone()),
                ..ProviderSettings::default()
            },
        };
        let result_id = id.clone();
        let api_key = api_key.trim().to_string();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            host_cx
                .unblock(move || {
                    let _ = std::fs::create_dir_all(&home);
                    let _ = std::fs::write(
                        home.join(".claude.json"),
                        r#"{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true}"#,
                    );
                })
                .await;
            host_cx.enqueue(move |state, cx| {
                if state.settings.profiles.contains_key(&id) {
                    return;
                }
                let mut settings = state.settings.clone();
                settings.profiles.insert(id.clone(), profile);
                state.update_settings(settings, cx);
                state.enqueue_store_write(
                    StoreWrite::SetProfileSecret {
                        profile_id: id.clone(),
                        key: "ANTHROPIC_API_KEY".to_string(),
                        value: Some(api_key),
                    },
                    cx,
                );
                state
                    .providers
                    .provider_secret_names
                    .entry(id.clone())
                    .or_default()
                    .insert("ANTHROPIC_API_KEY".to_string());
            });
        });
        result_id
    }

    /// Delete a user profile: remove its card, its secrets, and detach any
    /// sessions still pointing at it (they fall back to the built-in profile).
    /// Built-in ids are ignored.
    pub fn delete_profile(&mut self, id: &str, cx: &mut HostCx) {
        if Settings::is_builtin_profile_id(id) {
            return;
        }
        let mut settings = self.settings.clone();
        if settings.profiles.remove(id).is_none() {
            return;
        }
        self.enqueue_store_write(StoreWrite::ClearProfileSecrets(id.to_string()), cx);
        self.providers.provider_secret_names.remove(id);
        self.update_settings(settings, cx);
    }

    // -- provider status snapshots (Settings → Providers card) --------------

    #[allow(dead_code)]
    pub(crate) fn profile_snapshot(&self, id: &str) -> Option<&ProviderSnapshot> {
        self.providers.provider_snapshots.get(id)
    }

    #[allow(dead_code)]
    pub(crate) fn provider_snapshot(&self, provider: ProviderKind) -> Option<&ProviderSnapshot> {
        self.profile_snapshot(Settings::builtin_profile_id(provider))
    }

    /// The most recent probe time across providers (the section's "Checked …").
    /// Probe every provider profile: is the CLI there, what version, and who is signed
    /// in? Runs the same `--version` call the version check uses, plus the
    /// provider's own auth surface where one is unambiguous (`claude auth
    /// status --json`; Codex's `auth.json`). Multi-provider CLIs report an
    /// indeterminate auth state until their model/session requests run.
    pub fn refresh_provider_status(&mut self, cx: &mut HostCx) {
        for profile in self.all_profiles() {
            let profile_id = profile.id;
            let provider = profile.kind;
            let snapshot = self
                .providers
                .provider_snapshots
                .entry(profile_id.clone())
                .or_default();
            if snapshot.checking {
                continue;
            }
            snapshot.checking = true;
            let binary = self.resolve_profile_binary(&profile_id);
            let settings = self.settings.clone();
            let settings_store = self.settings_store.clone();
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let env_profile_id = profile_id.clone();
                let launch_env = host_cx
                    .unblock(move || {
                        let secrets = settings_store.profile_secrets(&env_profile_id);
                        launch_env_for_profile(&settings, &env_profile_id, secrets)
                    })
                    .await;
                let snapshot = probe_provider(provider, binary, launch_env).await;
                log::info!("probe {provider:?} profile {profile_id} -> {snapshot:?}");
                host_cx.enqueue(move |state, _cx| {
                    state
                        .providers
                        .provider_snapshots
                        .insert(profile_id, snapshot);
                });
            });
        }
    }

    /// Check every provider and the running tcode build in the background,
    /// storing results and toasting once for each newly available update.
    pub fn check_provider_versions(&mut self, cx: &mut HostCx) {
        for provider in NATIVE_PROVIDER_KINDS {
            let binary = self.resolve_provider_binary(provider);
            let status = self
                .providers
                .provider_versions
                .entry(provider)
                .or_default();
            if status.checking {
                continue;
            }
            status.checking = true;
            let program = binary
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| default_program(provider));
            let package = npm_package(provider);
            let settings = self.settings.clone();
            let settings_store = self.settings_store.clone();
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let profile_id = Settings::builtin_profile_id(provider).to_string();
                let env = host_cx
                    .unblock(move || {
                        let secrets = settings_store.profile_secrets(&profile_id);
                        launch_env_for_profile(&settings, &profile_id, secrets).pairs(provider)
                    })
                    .await;
                let installed = run_capture_env(&program, &["--version"], &env).await;
                let latest = run_capture("npm", &["view", package, "version"]).await;
                let assessment = host_cx
                    .unblock(move || {
                        provider_updates::check(ProviderCheckInput {
                            binary_path: binary.as_deref(),
                            installed_output: installed.as_deref(),
                            latest_output: latest.as_deref(),
                        })
                    })
                    .await;
                host_cx.enqueue(move |state, cx| {
                    let (installed, latest, source, update_available) = match assessment {
                        ProviderUpdateAssessment::UpToDate {
                            current,
                            latest,
                            install_source,
                        } => (Some(current), Some(latest), install_source, false),
                        ProviderUpdateAssessment::UpdateAvailable {
                            current,
                            latest,
                            install_source,
                        } => (Some(current), Some(latest), install_source, true),
                        ProviderUpdateAssessment::Unknown {
                            current,
                            latest,
                            install_source,
                            ..
                        } => {
                            // Unknown is deliberately silent, but it is not
                            // treated as evidence that the provider is current.
                            (current, latest, install_source, false)
                        }
                    };
                    let already = state
                        .providers
                        .provider_versions
                        .get(&provider)
                        .map(|s| s.update_available)
                        .unwrap_or(false);
                    let status = state
                        .providers
                        .provider_versions
                        .entry(provider)
                        .or_default();
                    status.checking = false;
                    status.install_source = source;
                    status.installed = installed;
                    status.latest = latest.clone();
                    status.update_available = update_available;
                    // Toast once when an update becomes newly available.
                    if update_available
                        && !already
                        && let Some(version) = &latest
                    {
                        emit_runtime(
                            cx,
                            RuntimeEvent::Notice(RuntimeNotice::UpdateAvailable {
                                provider,
                                version: version.clone(),
                            }),
                        );
                    }
                });
            });
        }

        if self.providers.tcode_update.checking {
            return;
        }
        self.providers.tcode_update.checking = true;
        let current = self.providers.tcode_update.current.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let fetched = host_cx.unblock(fetch_latest_tcode_release_json).await;
            let fetched = match &fetched {
                Ok(bytes) => Ok(bytes.as_slice()),
                Err(error) => Err(*error),
            };
            let assessment = app_releases::check(&current, fetched);
            host_cx.enqueue(move |state, cx| {
                let already = state.providers.tcode_update.update_available;
                let (latest, release_url, update_available) = match assessment {
                    AppReleaseAssessment::UpToDate {
                        latest,
                        release_url,
                        ..
                    } => (Some(latest), Some(release_url), false),
                    AppReleaseAssessment::UpdateAvailable {
                        latest,
                        release_url,
                        ..
                    } => (Some(latest), Some(release_url), true),
                    AppReleaseAssessment::Unknown {
                        latest,
                        release_url,
                        ..
                    } => {
                        // Network, policy-input, and response failures remain
                        // silent by an explicit runtime choice.
                        (latest, release_url, false)
                    }
                };
                let status = &mut state.providers.tcode_update;
                status.checking = false;
                status.latest = latest;
                status.release_url = release_url;
                status.update_available = update_available;
                if update_available
                    && !already
                    && let Some(version) = &status.latest
                {
                    emit_runtime(
                        cx,
                        RuntimeEvent::Notice(RuntimeNotice::TcodeUpdateAvailable {
                            version: version.clone(),
                        }),
                    );
                }
            });
        });
    }

    /// Run the provider's self-update command (per its detected install source),
    /// showing an "updating" toast, then re-check its version.
    pub fn update_provider(&mut self, provider: ProviderKind, cx: &mut HostCx) {
        let source = self
            .providers
            .provider_versions
            .get(&provider)
            .map(|s| s.install_source)
            .unwrap_or_default();
        let Some(command) = update_command(provider, source) else {
            self.report_error(RuntimeError::UpdateUnknown { provider }, cx);
            return;
        };
        let status = self
            .providers
            .provider_versions
            .entry(provider)
            .or_default();
        if status.updating {
            return;
        }
        status.updating = true;
        emit_runtime(
            cx,
            RuntimeEvent::Notice(RuntimeNotice::UpdatingProvider { provider }),
        );
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let args: Vec<&str> = command[1..].iter().map(String::as_str).collect();
            let ok = run_status(&command[0], &args).await;
            host_cx.enqueue(move |state, cx| {
                if let Some(status) = state.providers.provider_versions.get_mut(&provider) {
                    status.updating = false;
                }
                if ok {
                    emit_runtime(
                        cx,
                        RuntimeEvent::Notice(RuntimeNotice::UpdateDone { provider }),
                    );
                    // Refresh the version so the "update available" state clears.
                    state.check_provider_versions(cx);
                } else {
                    state.report_error(RuntimeError::UpdateFailed { provider }, cx);
                }
            });
        });
    }

    /// The copyable update command for a provider whose install source has
    /// already been detected. The install-source detail stays inside runtime.
    #[cfg(test)]
    pub(super) fn provider_update_command(&self, provider: ProviderKind) -> Option<String> {
        let source = self
            .providers
            .provider_versions
            .get(&provider)?
            .install_source;
        update_command_string(provider, source)
    }

    pub(super) fn cached_provider_commands(
        &self,
        provider: ProviderKind,
        acp_agent_id: Option<&str>,
    ) -> Vec<ProviderCommand> {
        self.store.load_commands(provider, acp_agent_id)
    }

    /// The cached model catalog for `provider` (empty when never fetched).
    pub(crate) fn models_for(&self, provider: ProviderKind) -> &[ModelSpec] {
        self.providers
            .model_catalogs
            .get(&provider)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn set_sidebar_collapsed(&mut self, collapsed: bool, cx: &mut HostCx) {
        // Persist so the choice survives a restart (save errors are cosmetic).
        self.settings.sidebar_collapsed = collapsed;
        self.persist_settings(cx);
    }
}

/// The reasoning-effort selection value, if any.
pub(super) fn effort_selection(selections: &[OptionSelection]) -> Option<String> {
    selections
        .iter()
        .find(|s| s.id == "reasoningEffort")
        .and_then(|s| s.value.as_str().map(str::to_string))
}

/// Selections sorted by id for order-independent comparison, optionally dropping
/// the reasoning-effort entry (which, for per-turn providers, never forces a
/// restart).
pub(super) fn normalized_selections(
    selections: &[OptionSelection],
    ignore_effort: bool,
) -> Vec<(String, serde_json::Value)> {
    let mut out: Vec<(String, serde_json::Value)> = selections
        .iter()
        .filter(|s| !(ignore_effort && s.id == "reasoningEffort"))
        .map(|s| (s.id.clone(), s.value.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub(super) fn launch_env_for_profile(
    settings: &Settings,
    profile_id: &str,
    secrets: std::collections::BTreeMap<String, String>,
) -> LaunchEnv {
    let Some(profile) = settings.resolved_profile(profile_id) else {
        return LaunchEnv::default();
    };
    let profile_settings = profile.settings;
    let env = profile_settings
        .env
        .iter()
        .filter(|var| !var.name.trim().is_empty())
        .filter_map(|var| {
            let value = if var.sensitive {
                // Sensitive rows keep their value only in secrets.json; a row
                // whose secret was never saved contributes nothing.
                secrets.get(&var.name).cloned()?
            } else {
                var.value.clone()
            };
            Some((var.name.trim().to_string(), value))
        })
        .collect();
    LaunchEnv {
        env,
        home: profile_settings.home_path.clone(),
    }
}

pub(super) fn provider_secret_names(
    settings: &Settings,
    settings_store: &SettingsStore,
) -> HashMap<String, HashSet<String>> {
    [
        ProviderKind::Codex,
        ProviderKind::ClaudeCode,
        ProviderKind::Pi,
        ProviderKind::OpenCode,
    ]
    .into_iter()
    .flat_map(|kind| settings.profiles_for_kind(kind))
    .map(|profile| {
        let id = profile.id;
        let names = launch_env_for_profile(settings, &id, settings_store.profile_secrets(&id))
            .env
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        (id, names)
    })
    .collect()
}

pub(super) fn session_launch_env(
    settings: &Settings,
    settings_store: &SettingsStore,
    meta: &SessionMeta,
) -> LaunchEnv {
    match meta.provider {
        ProviderKind::Acp => {}
        ProviderKind::Codex
        | ProviderKind::ClaudeCode
        | ProviderKind::Pi
        | ProviderKind::OpenCode => {
            let profile_id = meta
                .profile_id
                .clone()
                .unwrap_or_else(|| Settings::builtin_profile_id(meta.provider).to_string());
            let secrets = settings_store.profile_secrets(&profile_id);
            return launch_env_for_profile(settings, &profile_id, secrets);
        }
    }
    let env = meta
        .acp_agent_id
        .as_deref()
        .and_then(|id| settings.acp_agent(id))
        .map(|agent| agent.env.clone())
        .unwrap_or_default();
    LaunchEnv { env, home: None }
}

pub(super) fn session_options(
    meta: &SessionMeta,
    settings: &Settings,
    launch_env: LaunchEnv,
    mcp_server: Option<agent::McpRegistration>,
    orchestrate_server: Option<agent::McpRegistration>,
    orchestrate_report_server: Option<agent::McpRegistration>,
    computer_use_server: Option<agent::McpRegistration>,
) -> SessionOptions {
    // A session's binary / launch-args come from its selected profile (built-in
    // or user-created), so a third-party profile can point at its own CLI while
    // sharing the protocol adapter. Falls back to the kind's built-in card.
    let provider_settings = meta
        .profile_id
        .as_deref()
        .and_then(|id| settings.resolved_profile(id))
        .map(|profile| profile.settings)
        .unwrap_or_else(|| settings.provider(meta.provider));
    // For an ACP session, which agent to launch (and how) comes from the
    // installed-agent list, keyed by the id the session was created with.
    let acp_agent: Option<InstalledAgent> = meta
        .acp_agent_id
        .as_deref()
        .and_then(|id| settings.acp_agent(id))
        .cloned();
    let approval_mode = if meta
        .provider
        .caps()
        .downgrade_approval_without_native_approvals
        && !provider_settings.pi.native_approvals
    {
        match meta.approval_mode {
            ApprovalMode::Supervised | ApprovalMode::AutoAcceptEdits => ApprovalMode::FullAccess,
            ApprovalMode::ReadOnly => ApprovalMode::ReadOnly,
            ApprovalMode::FullAccess => ApprovalMode::FullAccess,
        }
    } else {
        meta.approval_mode
    };
    SessionOptions {
        cwd: meta.cwd.clone(),
        model: meta.model.clone(),
        abort_on_model_fallback: settings.abort_on_model_fallback,
        resume: meta.resume_cursor.clone(),
        fork: meta.pending_fork,
        binary_path: provider_settings.binary_path.clone(),
        approval_mode,
        option_selections: meta.option_selections.clone(),
        interaction_mode: meta.interaction_mode,
        mcp_servers: [
            meta.provider
                .caps()
                .mcp_servers
                .then_some(mcp_server)
                .flatten(),
            meta.orchestrate_enabled
                .then_some(orchestrate_server)
                .flatten(),
            meta.parent_session_id
                .is_some()
                .then_some(orchestrate_report_server)
                .flatten(),
            settings
                .computer_use
                .enabled
                .then_some(computer_use_server)
                .flatten(),
        ]
        .into_iter()
        .flatten()
        .collect(),
        launch_env,
        // Native providers that expose "Launch arguments" use their profile;
        // an ACP agent carries its own from the installed-agent card.
        extra_args: if meta.provider.caps().launch_args {
            match meta.provider {
                ProviderKind::ClaudeCode | ProviderKind::OpenCode => provider_settings.extra_args(),
                ProviderKind::Pi => {
                    let mut extra_args = provider_settings.extra_args();
                    if provider_settings.pi.trust_project_extensions {
                        extra_args.push("--approve".into());
                    }
                    extra_args
                }
                ProviderKind::Acp => acp_agent
                    .as_ref()
                    .map(|agent| agent.extra_args())
                    .unwrap_or_default(),
                ProviderKind::Codex => Vec::new(),
            }
        } else {
            Vec::new()
        },
        acp: acp_agent.map(|agent| agent::AcpAgent {
            id: agent.id.clone(),
            name: agent.name.clone(),
            launch: agent.launch.clone(),
        }),
    }
}
