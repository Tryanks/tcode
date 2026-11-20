use super::*;

impl WorkspaceStore {
    pub fn all_provider_profiles(&self) -> Vec<ResolvedProfile> {
        if !self.scope.is_full() {
            return self
                .scoped_providers
                .iter()
                .map(|choice| ResolvedProfile {
                    id: choice.profile_id.clone().unwrap_or_else(|| {
                        tcode_core::settings::provider_key(choice.provider).to_owned()
                    }),
                    kind: choice.provider,
                    settings: ProviderSettings {
                        enabled: true,
                        ..Default::default()
                    },
                    shared_env: Vec::new(),
                })
                .collect();
        }
        agent::ProviderKind::NATIVE
            .into_iter()
            .flat_map(|kind| self.settings_replica.profiles_for_kind(kind))
            .collect()
    }

    pub fn enabled_profiles(&self) -> Vec<ResolvedProfile> {
        self.all_provider_profiles()
            .into_iter()
            .filter(|profile| profile.settings.enabled)
            .collect()
    }

    pub(super) fn scoped_choice(&self, profile_id: &str) -> Option<&ScopedProviderChoice> {
        self.scoped_providers.iter().find(|choice| {
            choice
                .profile_id
                .as_deref()
                .unwrap_or(tcode_core::settings::provider_key(choice.provider))
                == profile_id
        })
    }

    pub fn profile_catalog(&self, profile_id: &str) -> Vec<agent::ModelSpec> {
        if !self.scope.is_full() {
            return self
                .scoped_choice(profile_id)
                .map(|choice| choice.models.clone())
                .unwrap_or_default();
        }
        self.settings_replica
            .resolved_profile(profile_id)
            .filter(ResolvedProfile::uses_native_model_catalog)
            .map(|profile| self.provider_model_catalog(profile.kind))
            .unwrap_or_default()
    }

    pub fn settings_installed_acp_agents(&self) -> Vec<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica
            .installed_acp_agents()
            .into_iter()
            .cloned()
            .collect()
    }

    pub fn providers_checked_at(&self) -> Option<u64> {
        self.providers_replica.providers_checked_at
    }

    pub fn providers_checking(&self) -> bool {
        self.providers_replica.providers_checking
    }

    /// The latest usage fetch for a provider profile, when one has landed.
    pub fn provider_usage(&self, profile_id: &str) -> Option<tcode_core::usage::ProviderUsage> {
        self.providers_replica
            .provider_usage
            .get(profile_id)
            .cloned()
    }

    /// A usage fetch is in flight for this profile.
    pub fn usage_checking(&self, profile_id: &str) -> bool {
        self.providers_replica.usage_checking.contains(profile_id)
    }

    /// The harness of `profile_id`; `None` for a profile the host does not
    /// list.
    pub fn provider_profile_kind(&self, profile_id: &str) -> Option<agent::ProviderKind> {
        if let Some(choice) = self.scoped_choice(profile_id) {
            return Some(choice.provider);
        }
        self.settings_replica
            .resolved_profile(profile_id)
            .map(|profile| profile.kind)
    }

    pub fn provider_profile_settings(&self, profile_id: &str) -> ProviderSettings {
        self.settings_replica
            .resolved_profile(profile_id)
            .map(|profile| profile.settings)
            .unwrap_or_default()
    }

    pub fn provider_model_catalog(&self, provider: agent::ProviderKind) -> Vec<agent::ModelSpec> {
        self.providers_replica
            .model_catalogs
            .get(&provider)
            .cloned()
            .unwrap_or_default()
    }

    /// The models a thread on `provider` picks from
    /// ([`tcode_protocol::ProvidersStatus::thread_models`]).
    pub fn thread_models(
        &self,
        provider: agent::ProviderKind,
        acp_agent_id: Option<&str>,
    ) -> &[agent::ModelSpec] {
        self.providers_replica.thread_models(provider, acp_agent_id)
    }

    /// Installed ACP agent `id`'s own models; none before it listed them.
    pub(crate) fn acp_models(&self, id: &str) -> &[agent::ModelSpec] {
        self.providers_replica
            .acp_models
            .get(id)
            .map_or(&[], Vec::as_slice)
    }

    /// Why the latest listing of `provider`'s models failed.
    pub(crate) fn models_error(&self, provider: agent::ProviderKind) -> Option<&str> {
        self.providers_replica
            .model_errors
            .get(&provider)
            .map(String::as_str)
    }

    pub(crate) fn models_loading(&self, provider: agent::ProviderKind) -> bool {
        self.providers_replica.models_loading.get(&provider) == Some(&true)
            && self
                .providers_replica
                .model_catalogs
                .get(&provider)
                .is_none_or(Vec::is_empty)
    }

    pub fn picker_models_for_profile(&self, profile_id: &str) -> Vec<ResolvedModel> {
        picker_models(
            &self.profile_catalog(profile_id),
            &self.provider_profile_settings(profile_id),
            &self.favorite_ids(profile_id),
        )
    }

    pub(super) fn favorite_ids(&self, profile_id: &str) -> Vec<String> {
        self.provider_profile_kind(profile_id)
            .map(|kind| self.settings_replica.favorite_ids(kind, profile_id))
            .unwrap_or_default()
    }

    pub fn provider_profile_display_name(&self, profile_id: &str) -> String {
        if let Some(choice) = self.scoped_choice(profile_id) {
            return choice.name.clone();
        }
        let email = self
            .provider_profile_snapshot(profile_id)
            .as_ref()
            .and_then(crate::provider_status::signed_in_account)
            .and_then(|auth| auth.email.clone());
        self.settings_replica
            .profile_display_name_as(profile_id, email.as_deref())
    }

    /// The profile as a list of its harness's profiles names it: what
    /// [`Self::provider_profile_display_name`] puts beside the product, or
    /// the whole name where there is nothing beside it.
    pub fn provider_profile_short_name(&self, profile_id: &str) -> String {
        if self.scoped_choice(profile_id).is_some() {
            return self.provider_profile_display_name(profile_id);
        }
        let email = self
            .provider_profile_snapshot(profile_id)
            .as_ref()
            .and_then(crate::provider_status::signed_in_account)
            .and_then(|auth| auth.email.clone());
        self.settings_replica
            .profile_qualifier_as(profile_id, email.as_deref())
            .unwrap_or_else(|| self.provider_profile_display_name(profile_id))
    }

    /// What the host says each harness is: the native ones, then each
    /// installed ACP agent.
    pub fn harness_descriptions(&self) -> &[tcode_protocol::HarnessDescription] {
        &self.providers_replica.harnesses
    }

    /// A user profile's own settings, without what it takes from its
    /// harness; `None` for a built-in profile.
    pub fn provider_profile_own_settings(&self, profile_id: &str) -> Option<ProviderSettings> {
        self.settings_replica
            .profiles
            .get(profile_id)
            .map(|profile| profile.settings.clone())
    }

    /// The threads that run on `profile_id` and fall back to the default
    /// account once it is deleted.
    pub fn threads_using_profile(&self, profile_id: &str) -> usize {
        self.index_replica
            .0
            .iter()
            .filter(|meta| meta.profile_id.as_deref() == Some(profile_id))
            .count()
    }

    pub fn provider_profile_supports_account_login(&self, profile_id: &str) -> bool {
        self.settings_replica
            .resolved_profile(profile_id)
            .is_some_and(|profile| profile.supports_account_login())
    }

    pub fn provider_profile_snapshot(&self, profile_id: &str) -> Option<ProviderSnapshot> {
        self.providers_replica
            .provider_snapshots
            .get(profile_id)
            .cloned()
    }

    pub fn provider_version_status(
        &self,
        provider: agent::ProviderKind,
    ) -> Option<ProviderVersionStatus> {
        self.providers_replica
            .provider_versions
            .get(&provider)
            .cloned()
    }

    pub fn provider_update_run(&self) -> Option<tcode_protocol::ProviderUpdateRun> {
        self.providers_replica.update_run.clone()
    }

    pub fn automatic_provider_updates(&self) -> Vec<agent::ProviderKind> {
        agent::ProviderKind::NATIVE
            .into_iter()
            .filter(|provider| {
                self.providers_replica
                    .provider_versions
                    .get(provider)
                    .is_some_and(|status| {
                        status.update_available
                            && status.update_command.is_some()
                            && !status.update_requires_terminal
                    })
            })
            .collect()
    }

    pub fn tcode_update_status(&self) -> tcode_protocol::TcodeUpdateStatus {
        self.providers_replica.tcode_update.clone()
    }

    /// The `0xRRGGBB` color a provider profile is drawn in; see
    /// [`Settings::provider_color`].
    pub fn provider_profile_color(&self, profile_id: &str) -> u32 {
        self.settings_replica.provider_color(profile_id)
    }

    /// Why the host's saved secrets could not be read.
    /// What of `provider`'s model catalog could not be read.
    pub fn model_problems(&self, provider: agent::ProviderKind) -> &[String] {
        self.providers_replica
            .model_problems
            .get(&provider)
            .map_or(&[], Vec::as_slice)
    }

    pub fn login_shell_unread(&self) -> Option<&str> {
        self.providers_replica.login_shell_unread.as_deref()
    }

    pub fn provider_secrets_unreadable(&self) -> Option<&str> {
        self.providers_replica.secrets_unreadable.as_deref()
    }

    pub fn provider_profile_stored_secret_names(&self, profile_id: &str) -> HashSet<String> {
        self.providers_replica
            .secret_names
            .get(profile_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn provider_dialog_models(
        &self,
        profile_id: &str,
        custom_models: &[String],
        hidden_models: &[String],
    ) -> Vec<ResolvedModel> {
        let mut settings = self.provider_profile_settings(profile_id);
        settings.custom_models = custom_models.to_vec();
        settings.hidden_models = hidden_models.to_vec();
        resolve_models(
            &self.profile_catalog(profile_id),
            &settings,
            &self.favorite_ids(profile_id),
        )
    }

    pub fn installed_acp_agent(
        &self,
        agent_id: &str,
    ) -> Option<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica.acp_agent(agent_id).cloned()
    }

    pub fn acp_marketplace_items(&self) -> Vec<AcpMarketplaceItem> {
        self.providers_replica.acp_marketplace_items.clone()
    }

    /// A profile's plugin catalog listed from `cwd`, or from outside any
    /// project when `cwd` is `None`.
    pub fn provider_plugin_catalog(
        &self,
        profile_id: &str,
        cwd: Option<&std::path::Path>,
    ) -> Option<&tcode_protocol::ProviderPluginCatalog> {
        self.providers_replica.plugins.iter().find(|catalog| {
            catalog.profile_id == profile_id && catalog.context_cwd.as_deref() == cwd
        })
    }

    /// Every plugin catalog the host holds for this client.
    pub fn provider_plugin_catalogs(&self) -> &[tcode_protocol::ProviderPluginCatalog] {
        &self.providers_replica.plugins
    }

    pub fn acp_registry_error(&self) -> Option<String> {
        self.providers_replica.acp_registry_error.clone()
    }

    pub fn acp_registry_loaded(&self) -> bool {
        self.providers_replica.acp_registry_loaded
    }

    pub fn acp_installing(&self, agent_id: &str) -> bool {
        self.providers_replica.acp_installing.contains(agent_id)
    }
}
