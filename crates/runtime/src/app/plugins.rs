use super::*;

use agent::{
    MarketplaceAction, PluginAction, PluginActionKind, PluginContext, PluginListing, PluginOp,
    PluginOpOutcome, PluginScope, list_plugins, run_plugin_op,
};
use tcode_protocol::{
    Command, PluginCatalogState, PluginChallenge, PluginChallengeKind, PluginOperationTarget,
    PluginStaleReason, ProviderPluginCatalog,
};

/// Which of a profile's two listings a catalog is: from outside any project,
/// where user scope is the profile's own, or from the one project a client
/// last asked about, whose project and local scopes every profile shares.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CatalogKey {
    profile_id: String,
    project: bool,
}

impl CatalogKey {
    fn new(profile_id: &str, cwd: Option<&PathBuf>) -> Self {
        Self {
            profile_id: profile_id.to_string(),
            project: cwd.is_some(),
        }
    }
}

/// One profile's last native plugin listing in one context. Nothing here is
/// persisted: the CLI's own files are the truth and are listed again on
/// demand.
pub(super) struct PluginCatalog {
    context_cwd: Option<PathBuf>,
    launch: PluginLaunch,
    listing: PluginListing,
    state: PluginCatalogState,
    /// The listing running, which stops when this is dropped.
    listing_task: Option<HostTask<()>>,
    /// Bumped when the context or the profile's CLI/home changes; listings
    /// and challenges from an older generation are discarded.
    generation: u64,
    /// Only the newest listing request may land.
    list_request: u64,
    /// The one mutation of this profile in flight or waiting on its user.
    operation: Option<PluginOperation>,
    /// Native facts a finished operation reported, shown on its entry until
    /// the entry is changed again or the context changes.
    notes: HashMap<String, Vec<(String, String)>>,
}

/// The native installation a profile's plugin commands address.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginLaunch {
    provider: ProviderKind,
    binary_path: Option<PathBuf>,
    home: Option<PathBuf>,
}

struct PluginOperation {
    id: RuntimeOperationId,
    subject: String,
    target: PluginOperationTarget,
    op: PluginOp,
    cwd: Option<PathBuf>,
    generation: u64,
    started: bool,
    challenge: Option<PluginChallenge>,
    /// The harness's command while it runs; dropping it stops the command.
    running: Option<HostTask<()>>,
}

fn error(code: &str, message: &str) -> ProtocolError {
    ProtocolError {
        code: code.into(),
        message: message.into(),
    }
}

fn native_message(error: AgentError) -> String {
    match error {
        AgentError::Provider(message) => message,
        other => other.to_string(),
    }
}

impl AppState {
    fn plugin_launch(&self, profile_id: &str) -> Option<PluginLaunch> {
        let profile = self.settings.resolved_profile(profile_id)?;
        Some(PluginLaunch {
            provider: profile.kind,
            binary_path: profile.settings.binary_path,
            home: profile.settings.home_path,
        })
    }

    pub fn refresh_provider_plugins(
        &mut self,
        profile_id: &str,
        cwd: Option<PathBuf>,
        cx: &mut HostCx,
    ) {
        let Some(launch) = self
            .plugin_launch(profile_id)
            .filter(|launch| launch.provider.caps().plugin_management.manages())
        else {
            return;
        };
        let key = CatalogKey::new(profile_id, cwd.as_ref());
        let catalog = self
            .plugin_catalogs
            .entry(key.clone())
            .or_insert_with(|| PluginCatalog {
                context_cwd: cwd.clone(),
                launch: launch.clone(),
                listing: PluginListing::default(),
                state: PluginCatalogState::Stale {
                    reason: PluginStaleReason::NotLoaded,
                },
                listing_task: None,
                generation: 0,
                list_request: 0,
                operation: None,
                notes: HashMap::new(),
            });
        if catalog.context_cwd != cwd || catalog.launch != launch {
            catalog.context_cwd = cwd;
            catalog.launch = launch;
            catalog.generation += 1;
            catalog.notes.clear();
            if matches!(catalog.state, PluginCatalogState::Fresh) {
                catalog.state = PluginCatalogState::Stale {
                    reason: PluginStaleReason::ContextChanged,
                };
            }
            if let Some(operation) = catalog
                .operation
                .take_if(|operation| operation.challenge.is_some())
            {
                Self::abandon_challenge(profile_id, operation, cx);
            }
        }
        self.list_plugins(&key, cx);
    }

    /// A challenge that can no longer be answered ends its operation with
    /// the CLI's own refusal, which is what happened natively.
    fn abandon_challenge(profile_id: &str, operation: PluginOperation, cx: &mut HostCx) {
        if !operation.started {
            return;
        }
        let detail = operation
            .challenge
            .and_then(|challenge| challenge.native_text)
            .unwrap_or_default();
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::PluginOperationFailed {
                operation: operation.id,
                profile_id: profile_id.to_string(),
                target: operation.target,
                detail,
            }),
        );
    }

    fn list_plugins(&mut self, key: &CatalogKey, cx: &mut HostCx) {
        let Some(catalog) = self.plugin_catalogs.get_mut(key) else {
            return;
        };
        catalog.list_request += 1;
        let request = catalog.list_request;
        let provider = catalog.launch.provider;
        let cwd = catalog.context_cwd.clone();
        let context = self.plugin_context(&key.profile_id, cwd, cx);
        let owner = key.clone();
        let key = key.clone();
        let host_cx = cx.clone();
        let task = cx.spawn_background(async move {
            let result = match context.await {
                Ok(context) => list_plugins(provider, &context).await,
                Err(error) => Err(error),
            };
            host_cx.enqueue(move |state, _cx| {
                let Some(catalog) = state.plugin_catalogs.get_mut(&key) else {
                    return;
                };
                if catalog.list_request != request {
                    return;
                }
                catalog.listing_task = None;
                match result {
                    Ok(listing) => {
                        catalog.listing = listing;
                        catalog.state = PluginCatalogState::Fresh;
                    }
                    Err(error) => {
                        catalog.state = PluginCatalogState::Error {
                            message: native_message(error),
                        }
                    }
                }
            });
        });
        // A listing started while another runs replaces it, which stops it.
        if let Some(catalog) = self.plugin_catalogs.get_mut(&owner) {
            catalog.listing_task = Some(task);
        }
    }

    /// Stop the profile's listings still running. What each catalog last
    /// listed stays, marked stopped, until it is listed again.
    pub(crate) fn stop_plugin_listing(&mut self, profile_id: &str) {
        for (key, catalog) in self.plugin_catalogs.iter_mut() {
            if key.profile_id == profile_id && catalog.listing_task.take().is_some() {
                catalog.list_request += 1;
                catalog.state = PluginCatalogState::Stale {
                    reason: PluginStaleReason::Stopped,
                };
            }
        }
    }

    /// Check a plugin command against the catalog it was offered from, so a
    /// client acting on an old snapshot cannot run what the host no longer
    /// offers.
    pub(super) fn validate_plugin_command(&self, command: &Command) -> Result<(), ProtocolError> {
        let (profile_id, cwd) = match command {
            Command::RefreshProviderPlugins { profile_id, .. } => (profile_id, None),
            Command::InstallProviderPlugin {
                profile_id, cwd, ..
            }
            | Command::UninstallProviderPlugin {
                profile_id, cwd, ..
            }
            | Command::SetProviderPluginEnabled {
                profile_id, cwd, ..
            }
            | Command::UpdateProviderPlugin {
                profile_id, cwd, ..
            }
            | Command::AddProviderMarketplace {
                profile_id, cwd, ..
            }
            | Command::RemoveProviderMarketplace {
                profile_id, cwd, ..
            } => (profile_id, Some(cwd)),
            Command::ResolvePluginChallenge { op_id, .. } => {
                let pending = self.plugin_catalogs.values().any(|catalog| {
                    catalog
                        .operation
                        .as_ref()
                        .and_then(|operation| operation.challenge.as_ref())
                        .is_some_and(|challenge| challenge.op_id == *op_id)
                });
                return if pending {
                    Ok(())
                } else {
                    Err(error(
                        "unknown_plugin_challenge",
                        "This confirmation is no longer pending.",
                    ))
                };
            }
            _ => return Ok(()),
        };
        let Some(launch) = self.plugin_launch(profile_id) else {
            return Err(error(
                "unknown_profile",
                "This provider profile is no longer available.",
            ));
        };
        let management = launch.provider.caps().plugin_management;
        if !management.manages() {
            return Err(error(
                "plugin_management_unsupported",
                "This provider's plugins cannot be managed from Tcode.",
            ));
        }
        let Some(cwd) = cwd else {
            return Ok(());
        };
        let catalog = self
            .plugin_catalogs
            .get(&CatalogKey::new(profile_id, cwd.as_ref()))
            .filter(|catalog| catalog.context_cwd == *cwd && catalog.launch == launch)
            .ok_or_else(|| {
                error(
                    "plugin_catalog_stale",
                    "List this provider's plugins again before changing them.",
                )
            })?;
        // Both of a profile's listings change the same home, so one change
        // runs at a time.
        if self
            .plugin_catalogs
            .iter()
            .any(|(key, catalog)| key.profile_id == *profile_id && catalog.operation.is_some())
        {
            return Err(error(
                "plugin_operation_in_flight",
                "Another plugin change for this provider is still running.",
            ));
        }
        let offered = |entry_id: &str, action: PluginAction| {
            management.supports(action.kind())
                && catalog
                    .listing
                    .entries
                    .iter()
                    .any(|entry| entry.id == entry_id && entry.actions.contains(&action))
        };
        let available = match command {
            Command::InstallProviderPlugin {
                entry_id, scope, ..
            } if management.source_install => {
                !entry_id.trim().is_empty() && management.scopes.contains(scope)
            }
            Command::InstallProviderPlugin {
                entry_id, scope, ..
            } => offered(entry_id, PluginAction::Install { scope: *scope }),
            Command::UninstallProviderPlugin {
                entry_id, scope, ..
            } => offered(entry_id, PluginAction::Uninstall { scope: *scope }),
            Command::SetProviderPluginEnabled {
                entry_id,
                scope,
                enabled: true,
                ..
            } => offered(entry_id, PluginAction::Enable { scope: *scope }),
            Command::SetProviderPluginEnabled {
                entry_id,
                scope,
                enabled: false,
                ..
            } => offered(entry_id, PluginAction::Disable { scope: *scope }),
            Command::UpdateProviderPlugin {
                entry_id, scope, ..
            } => offered(entry_id, PluginAction::Update { scope: *scope }),
            Command::AddProviderMarketplace { source, .. } => {
                management.marketplaces
                    && !source.trim().is_empty()
                    && catalog
                        .listing
                        .marketplace_actions
                        .contains(&MarketplaceAction::Add)
            }
            Command::RemoveProviderMarketplace { name, .. } => {
                management.marketplaces && removal(&catalog.listing, name).is_some()
            }
            _ => true,
        };
        if available {
            Ok(())
        } else {
            Err(error(
                "plugin_action_unavailable",
                "This plugin action is not available here.",
            ))
        }
    }

    pub fn start_plugin_operation(
        &mut self,
        profile_id: &str,
        cwd: Option<PathBuf>,
        op: PluginOp,
        cx: &mut HostCx,
    ) {
        let (subject, target) = match &op {
            PluginOp::Install { id, scope, .. } => {
                plugin_target(id, *scope, PluginActionKind::Install)
            }
            PluginOp::Uninstall { id, scope } => {
                plugin_target(id, *scope, PluginActionKind::Uninstall)
            }
            PluginOp::SetEnabled { id, scope, enabled } => plugin_target(
                id,
                *scope,
                if *enabled {
                    PluginActionKind::Enable
                } else {
                    PluginActionKind::Disable
                },
            ),
            PluginOp::Update { id, scope, .. } => {
                plugin_target(id, *scope, PluginActionKind::Update)
            }
            PluginOp::AddMarketplace { source } => (
                source.clone(),
                PluginOperationTarget::AddMarketplace {
                    source: source.clone(),
                },
            ),
            PluginOp::RemoveMarketplace { name } => (
                name.clone(),
                PluginOperationTarget::RemoveMarketplace { name: name.clone() },
            ),
        };
        let key = CatalogKey::new(profile_id, cwd.as_ref());
        let id = self.next_operation_id();
        // Native removal has no confirmation of its own, so ask first.
        let challenge = match &op {
            PluginOp::RemoveMarketplace { name } => {
                let uninstalls = self
                    .plugin_catalogs
                    .get(&key)
                    .and_then(|catalog| removal(&catalog.listing, name))
                    .unwrap_or_default();
                Some(PluginChallenge {
                    op_id: self.next_operation_id(),
                    profile_id: profile_id.to_string(),
                    entry_id: name.clone(),
                    kind: PluginChallengeKind::ConfirmDestructive { uninstalls },
                    native_text: None,
                })
            }
            _ => None,
        };
        let Some(catalog) = self.plugin_catalogs.get_mut(&key) else {
            return;
        };
        let wait = challenge.is_some();
        catalog.operation = Some(PluginOperation {
            id,
            subject,
            target,
            op,
            cwd,
            generation: catalog.generation,
            started: false,
            challenge,
            running: None,
        });
        if !wait {
            self.execute_plugin_operation(&key, cx);
        }
    }

    fn execute_plugin_operation(&mut self, key: &CatalogKey, cx: &mut HostCx) {
        let profile_id = key.profile_id.as_str();
        let Some(catalog) = self.plugin_catalogs.get_mut(key) else {
            return;
        };
        let provider = catalog.launch.provider;
        let Some(operation) = catalog.operation.as_mut() else {
            return;
        };
        if !operation.started {
            operation.started = true;
            emit_runtime(
                cx,
                RuntimeEvent::Toast(RuntimeToast::PluginOperationStarted {
                    operation: operation.id,
                    profile_id: profile_id.to_string(),
                    target: operation.target.clone(),
                }),
            );
        }
        let id = operation.id;
        let op = operation.op.clone();
        let cwd = operation.cwd.clone();
        let context = self.plugin_context(profile_id, cwd, cx);
        let finished = key.clone();
        let host_cx = cx.clone();
        let running = cx.spawn_background(async move {
            let result = match context.await {
                Ok(context) => run_plugin_op(provider, &context, &op).await,
                Err(error) => Err(error),
            };
            host_cx.enqueue(move |state, cx| {
                state.finish_plugin_operation(&finished, id, result, cx);
            });
        });
        if let Some(operation) = self
            .plugin_catalogs
            .get_mut(key)
            .and_then(|catalog| catalog.operation.as_mut())
        {
            operation.running = Some(running);
        }
    }

    /// Stop the profile's running change: its command and what that started
    /// are killed, and the catalogs it touched are listed again, since the
    /// harness may have changed part of what it was asked.
    pub(crate) fn cancel_plugin_operation(&mut self, profile_id: &str, cx: &mut HostCx) {
        let Some((key, operation)) = self.plugin_catalogs.iter_mut().find_map(|(key, catalog)| {
            (key.profile_id == profile_id)
                .then(|| {
                    catalog
                        .operation
                        .take_if(|operation| operation.running.is_some())
                })
                .flatten()
                .map(|operation| (key.clone(), operation))
        }) else {
            return;
        };
        let shared = shared_scope(&operation.op)
            .then(|| operation.cwd.clone())
            .flatten();
        drop(operation.running);
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::PluginOperationCancelled {
                operation: operation.id,
                profile_id: profile_id.to_string(),
                target: operation.target,
            }),
        );
        self.after_plugin_change(&key, shared, cx);
    }

    fn finish_plugin_operation(
        &mut self,
        key: &CatalogKey,
        id: RuntimeOperationId,
        result: Result<PluginOpOutcome, AgentError>,
        cx: &mut HostCx,
    ) {
        let profile_id = key.profile_id.as_str();
        let challenge_id = self.next_operation_id();
        let Some(catalog) = self.plugin_catalogs.get_mut(key) else {
            return;
        };
        let Some(mut operation) = catalog
            .operation
            .take_if(|operation| operation.id == id && operation.challenge.is_none())
        else {
            return;
        };
        let target = operation.target.clone();
        let shared = shared_scope(&operation.op)
            .then(|| operation.cwd.clone())
            .flatten();
        let toast = match result {
            Ok(PluginOpOutcome::AcceptCommand(acceptance))
                if operation.generation == catalog.generation =>
            {
                operation.challenge = Some(PluginChallenge {
                    op_id: challenge_id,
                    profile_id: profile_id.to_string(),
                    entry_id: operation.subject.clone(),
                    kind: PluginChallengeKind::AcceptCommand {
                        command: acceptance.command,
                        sha256: acceptance.sha256,
                        mode: acceptance.mode,
                    },
                    native_text: Some(acceptance.native_text),
                });
                operation.running = None;
                catalog.operation = Some(operation);
                return;
            }
            Ok(PluginOpOutcome::AcceptCommand(acceptance)) => RuntimeToast::PluginOperationFailed {
                operation: id,
                profile_id: profile_id.to_string(),
                target,
                detail: acceptance.native_text,
            },
            Ok(PluginOpOutcome::Done { diagnostics }) => {
                if diagnostics.is_empty() {
                    catalog.notes.remove(&operation.subject);
                } else {
                    catalog.notes.insert(operation.subject.clone(), diagnostics);
                }
                RuntimeToast::PluginOperationSucceeded {
                    operation: id,
                    profile_id: profile_id.to_string(),
                    target,
                }
            }
            Err(error) => {
                catalog.notes.remove(&operation.subject);
                RuntimeToast::PluginOperationFailed {
                    operation: id,
                    profile_id: profile_id.to_string(),
                    target,
                    detail: native_message(error),
                }
            }
        };
        emit_runtime(cx, RuntimeEvent::Toast(toast));
        self.after_plugin_change(key, shared, cx);
    }

    /// A finished native change: menus cached for the same home no longer
    /// describe it, and every catalog listing that home is out of date. A
    /// change to a project's own scope is shared with every profile of the
    /// provider, so their listings of that project are out of date too.
    fn after_plugin_change(&mut self, key: &CatalogKey, project: Option<PathBuf>, cx: &mut HostCx) {
        let Some(launch) = self
            .plugin_catalogs
            .get(key)
            .map(|catalog| catalog.launch.clone())
        else {
            return;
        };
        self.enqueue_store_write(
            StoreWrite::InvalidateCommands(CommandsCacheKey::Native {
                provider: launch.provider,
                home: launch.home.clone(),
            }),
            cx,
        );
        let affected: Vec<CatalogKey> = self
            .plugin_catalogs
            .iter_mut()
            .filter(|(_, catalog)| {
                catalog.launch.provider == launch.provider
                    && (catalog.launch.home == launch.home
                        || (project.is_some() && catalog.context_cwd == project))
            })
            .map(|(id, catalog)| {
                if !matches!(catalog.state, PluginCatalogState::Error { .. }) {
                    catalog.state = PluginCatalogState::Stale {
                        reason: PluginStaleReason::Changed,
                    };
                }
                id.clone()
            })
            .collect();
        for key in affected {
            self.list_plugins(&key, cx);
        }
    }

    /// Answer a pending challenge. Acceptance applies only to the command or
    /// removal this challenge showed; the hash never comes from the client.
    pub fn resolve_plugin_challenge(
        &mut self,
        op_id: RuntimeOperationId,
        accept: bool,
        cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        let unknown = || {
            error(
                "unknown_plugin_challenge",
                "This confirmation is no longer pending.",
            )
        };
        let key = self
            .plugin_catalogs
            .iter()
            .find(|(_, catalog)| {
                catalog
                    .operation
                    .as_ref()
                    .and_then(|operation| operation.challenge.as_ref())
                    .is_some_and(|challenge| challenge.op_id == op_id)
            })
            .map(|(key, _)| key.clone())
            .ok_or_else(unknown)?;
        let profile_id = key.profile_id.clone();
        let current = self.plugin_launch(&profile_id);
        let next_challenge_id = self.next_operation_id();
        let catalog = self.plugin_catalogs.get_mut(&key).ok_or_else(unknown)?;
        let mut operation = catalog.operation.take().ok_or_else(unknown)?;
        if operation.generation != catalog.generation || current.as_ref() != Some(&catalog.launch) {
            Self::abandon_challenge(&profile_id, operation, cx);
            return Err(unknown());
        }
        if !accept {
            Self::abandon_challenge(&profile_id, operation, cx);
            return Ok(());
        }
        let challenge = operation.challenge.take().ok_or_else(unknown)?;
        match challenge.kind {
            PluginChallengeKind::AcceptCommand { sha256, .. } => match &mut operation.op {
                PluginOp::Install { accept_command, .. }
                | PluginOp::Update { accept_command, .. } => *accept_command = Some(sha256),
                _ => return Err(unknown()),
            },
            PluginChallengeKind::ConfirmDestructive { uninstalls } => {
                let current = removal(&catalog.listing, &operation.subject).unwrap_or_default();
                if current != uninstalls {
                    operation.challenge = Some(PluginChallenge {
                        op_id: next_challenge_id,
                        kind: PluginChallengeKind::ConfirmDestructive {
                            uninstalls: current,
                        },
                        ..challenge
                    });
                    catalog.operation = Some(operation);
                    return Err(error(
                        "plugin_challenge_changed",
                        "What this removes has changed; confirm it again.",
                    ));
                }
            }
        }
        catalog.operation = Some(operation);
        self.execute_plugin_operation(&key, cx);
        Ok(())
    }

    pub(crate) fn provider_plugin_catalogs(&self) -> Vec<ProviderPluginCatalog> {
        let mut catalogs: Vec<ProviderPluginCatalog> = self
            .plugin_catalogs
            .iter()
            .filter(|(key, _)| self.settings.resolved_profile(&key.profile_id).is_some())
            .map(|(key, catalog)| ProviderPluginCatalog {
                profile_id: key.profile_id.clone(),
                context_cwd: catalog.context_cwd.clone(),
                marketplaces: catalog.listing.marketplaces.clone(),
                marketplace_actions: catalog.listing.marketplace_actions.clone(),
                errors: catalog.listing.errors.clone(),
                entries: catalog
                    .listing
                    .entries
                    .iter()
                    .map(|entry| {
                        let mut entry = entry.clone();
                        if let Some(notes) = catalog.notes.get(&entry.id) {
                            entry.diagnostics.extend(notes.iter().cloned());
                        }
                        entry
                    })
                    .collect(),
                state: if self.plugin_launch(&key.profile_id).as_ref() == Some(&catalog.launch) {
                    catalog.state.clone()
                } else {
                    PluginCatalogState::Stale {
                        reason: PluginStaleReason::ContextChanged,
                    }
                },
                loading: catalog.listing_task.is_some(),
                pending: catalog
                    .operation
                    .iter()
                    .map(|operation| operation.subject.clone())
                    .collect(),
                challenges: catalog
                    .operation
                    .iter()
                    .filter_map(|operation| operation.challenge.clone())
                    .collect(),
            })
            .collect();
        catalogs
            .sort_by(|a, b| (&a.profile_id, &a.context_cwd).cmp(&(&b.profile_id, &b.context_cwd)));
        catalogs
    }
}

fn plugin_target(
    id: &str,
    scope: PluginScope,
    action: PluginActionKind,
) -> (String, PluginOperationTarget) {
    (
        id.to_string(),
        PluginOperationTarget::Plugin {
            action,
            scope,
            plugin: id.to_string(),
        },
    )
}

/// Whether `op` changes a project's own scope, which every profile shares.
fn shared_scope(op: &PluginOp) -> bool {
    match op {
        PluginOp::Install { scope, .. }
        | PluginOp::Uninstall { scope, .. }
        | PluginOp::SetEnabled { scope, .. }
        | PluginOp::Update { scope, .. } => {
            matches!(scope, PluginScope::Project | PluginScope::Local)
        }
        PluginOp::AddMarketplace { .. } | PluginOp::RemoveMarketplace { .. } => false,
    }
}

/// What removing `name` uninstalls, when the listing offers its removal.
fn removal(listing: &PluginListing, name: &str) -> Option<Vec<String>> {
    listing
        .marketplace_actions
        .iter()
        .find_map(|action| match action {
            MarketplaceAction::Remove {
                marketplace,
                uninstalls,
            } if marketplace == name => Some(uninstalls.clone()),
            _ => None,
        })
}

impl AppState {
    /// Resolve the native plugin installation without session arguments or MCP servers.
    /// Without a project, run from the provider home instead of a project directory.
    fn plugin_context(
        &self,
        profile_id: &str,
        cwd: Option<PathBuf>,
        cx: &HostCx,
    ) -> impl std::future::Future<Output = Result<PluginContext, AgentError>> + use<> {
        let binary_path = self
            .settings
            .resolved_profile(profile_id)
            .and_then(|profile| profile.settings.binary_path);
        let launch_env = self.prepare_profile_env(profile_id, cx);
        async move {
            let launch_env = launch_env.await.map_err(AgentError::Provider)?;
            let project = cwd.is_some();
            let cwd = cwd
                .or_else(|| launch_env.home.clone())
                .or_else(std::env::home_dir)
                .unwrap_or_else(std::env::temp_dir);
            Ok(PluginContext {
                binary_path,
                launch_env,
                cwd,
                project,
            })
        }
    }
}
