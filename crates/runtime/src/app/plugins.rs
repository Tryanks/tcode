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
    loading: bool,
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
                loading: false,
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
        catalog.loading = true;
        let request = catalog.list_request;
        let provider = catalog.launch.provider;
        let cwd = catalog.context_cwd.clone();
        let context = self.plugin_context(&key.profile_id, cwd, cx);
        let key = key.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
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
                catalog.loading = false;
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
        let key = key.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = match context.await {
                Ok(context) => run_plugin_op(provider, &context, &op).await,
                Err(error) => Err(error),
            };
            host_cx.enqueue(move |state, cx| {
                state.finish_plugin_operation(&key, id, result, cx);
            });
        });
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
                loading: catalog.loading,
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::app::test_support::*;
    use tcode_protocol::{CommandResponse, HostMessage};

    const GAMMA_SHA: &str = "b9c02c85b17261fa1fce010664bb16cbe866f14c7deeaf6cb6c37c2736bbe269";

    /// A `claude` that answers the recorded probe outputs and logs each
    /// invocation as `<cwd> <argv…>`.
    struct FakeClaude {
        dir: PathBuf,
        binary: PathBuf,
        log: PathBuf,
        home: PathBuf,
        project: PathBuf,
    }

    impl FakeClaude {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let dir =
                std::env::temp_dir().join(format!("tcode-fake-claude-{}", uuid::Uuid::new_v4()));
            let home = dir.join("home");
            let project = dir.join("project");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&project).unwrap();
            let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../agent/tests/fixtures/plugins/claude");
            let log = dir.join("argv.log");
            let binary = dir.join("claude");
            fs::write(
                &binary,
                format!(
                    "#!/bin/sh\n\
                     F='{fixtures}'\n\
                     printf '%s %s\\n' \"$PWD\" \"$*\" >> '{log}'\n\
                     case \"$*\" in\n\
                     'plugin list --json --available') cat \"$F/plugin-list-available-after-install.json\" ;;\n\
                     'plugin marketplace list --json') cat \"$F/marketplace-list.json\" ;;\n\
                     'plugin details '*) cat \"$F/details.txt\" ;;\n\
                     *'--accept-command {GAMMA_SHA}') cat \"$F/install-command-source-accepted.json\" ;;\n\
                     'plugin install gamma@tcode-probe '*) cat \"$F/install-command-source-challenge.txt\"; exit 1 ;;\n\
                     'plugin disable alpha@tcode-probe --scope user --json') cat \"$F/disable-scope-user.json\" ;;\n\
                     *) echo \"unexpected: $*\" >&2; exit 2 ;;\n\
                     esac\n",
                    fixtures = fixtures.display(),
                    log = log.display(),
                ),
            )
            .unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                dir,
                binary,
                log,
                home,
                project,
            }
        }

        fn calls(&self, verb: &str) -> Vec<String> {
            fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains(verb))
                .map(str::to_string)
                .collect()
        }
    }

    impl Drop for FakeClaude {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// A host whose Claude profile runs `fake`.
    fn host_with(fake: &FakeClaude, store: &TestStore) -> TestClientState {
        let mut state = TestClientState::new((**store).clone());
        let claude = state.settings.provider_mut(ProviderKind::ClaudeCode);
        claude.binary_path = Some(fake.binary.clone());
        claude.home_path = Some(fake.home.clone());
        state
    }

    /// The default profile's catalog listed from a project.
    fn catalog(state: &TestClientState) -> Option<ProviderPluginCatalog> {
        state
            .provider_plugin_catalogs()
            .into_iter()
            .find(|catalog| catalog.profile_id == "claude" && catalog.context_cwd.is_some())
    }

    fn settled(state: &TestClientState) -> bool {
        catalog(state).is_some_and(|catalog| {
            !catalog.loading
                && catalog.pending.is_empty()
                && matches!(catalog.state, PluginCatalogState::Fresh)
        })
    }

    fn challenged(state: &TestClientState) -> bool {
        catalog(state).is_some_and(|catalog| !catalog.challenges.is_empty())
    }

    fn ack(cx: &mut TestAppContext, id: u64) -> Result<CommandResponse, ProtocolError> {
        cx.drain_outgoing()
            .into_iter()
            .find_map(|message| match message {
                HostMessage::Ack { id: acked, result } if acked == id => Some(result),
                _ => None,
            })
            .expect("command acknowledged")
    }

    /// A harness without a catalog installs what a person typed, which no
    /// listing could have offered, in a scope it has and no other.
    #[test]
    fn a_typed_package_source_is_installed_in_a_scope_the_harness_has() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("tcode-fake-pi-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("argv.log");
        let binary = dir.join("pi");
        fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\necho 'No packages installed.'\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let store = TestStore::new("tcode-plugin-source");
        let cx = &mut TestAppContext::default();
        let mut host = TestClientState::new((*store).clone());
        host.settings.provider_mut(ProviderKind::Pi).binary_path = Some(binary);
        let state = cx.new_entity(host);
        state.dispatch_command(
            cx,
            1,
            Command::RefreshProviderPlugins {
                profile_id: "pi".into(),
                cwd: Some(dir.clone()),
            },
        );
        let settled = |state: &TestClientState| {
            state.provider_plugin_catalogs().iter().any(|catalog| {
                catalog.profile_id == "pi"
                    && !catalog.loading
                    && catalog.pending.is_empty()
                    && matches!(catalog.state, PluginCatalogState::Fresh)
            })
        };
        cx.run_until(settled);
        let install = |scope| Command::InstallProviderPlugin {
            profile_id: "pi".into(),
            entry_id: "npm:typed".into(),
            scope,
            cwd: Some(dir.clone()),
        };
        state.dispatch_command(cx, 2, install(PluginScope::Local));
        cx.run_until_parked();
        assert_eq!(ack(cx, 2).unwrap_err().code, "plugin_action_unavailable");
        state.dispatch_command(cx, 3, install(PluginScope::Project));
        cx.run_until(|state| {
            settled(state)
                && fs::read_to_string(&log)
                    .is_ok_and(|calls| calls.contains("install npm:typed -l --approve"))
        });
        assert_eq!(ack(cx, 3).unwrap(), CommandResponse::Unit);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn accepting_a_command_challenge_reruns_with_its_hash_and_a_stale_one_is_refused() {
        let fake = FakeClaude::new();
        let store = TestStore::new("tcode-plugin-challenge");
        let cx = &mut TestAppContext::default();
        let state = cx.new_entity(host_with(&fake, &store));
        let project = Some(fake.project.clone());
        let refresh = |cwd: Option<PathBuf>| Command::RefreshProviderPlugins {
            profile_id: "claude".into(),
            cwd,
        };
        let install = Command::InstallProviderPlugin {
            profile_id: "claude".into(),
            entry_id: "gamma@tcode-probe".into(),
            scope: PluginScope::User,
            cwd: project.clone(),
        };

        state.dispatch_command(cx, 1, refresh(project.clone()));
        cx.run_until(settled);
        state.dispatch_command(cx, 2, install.clone());
        cx.run_until(challenged);
        let challenge = state.read(|state| catalog(state).unwrap().challenges.remove(0));
        assert_eq!(
            challenge.kind,
            PluginChallengeKind::AcceptCommand {
                command: "echo /tmp/tcode-probe/gamma-src".into(),
                sha256: GAMMA_SHA.into(),
                mode: Some("copy".into()),
            }
        );

        // Listing the project slot from another project abandons the
        // challenge it showed.
        let elsewhere = fake.dir.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        state.dispatch_command(cx, 3, refresh(Some(elsewhere)));
        cx.run_until(settled);
        state.dispatch_command(
            cx,
            4,
            Command::ResolvePluginChallenge {
                op_id: challenge.op_id,
                accept: true,
            },
        );
        cx.run_until_parked();
        assert_eq!(ack(cx, 4).unwrap_err().code, "unknown_plugin_challenge");
        assert_eq!(fake.calls("plugin install").len(), 1);

        state.dispatch_command(cx, 5, refresh(project.clone()));
        cx.run_until(settled);
        state.dispatch_command(cx, 6, install);
        cx.run_until(challenged);
        let challenge = state.read(|state| catalog(state).unwrap().challenges.remove(0));
        state.dispatch_command(
            cx,
            7,
            Command::ResolvePluginChallenge {
                op_id: challenge.op_id,
                accept: true,
            },
        );
        cx.run_until(|state| settled(state) && fake.calls("--accept-command").len() == 1);
        assert_eq!(ack(cx, 7).unwrap(), CommandResponse::Unit);
        let installs = fake.calls("plugin install");
        let accepted = installs.last().unwrap();
        assert!(
            accepted.ends_with(&format!(
                "plugin install gamma@tcode-probe --scope user --json --accept-command {GAMMA_SHA}"
            )),
            "{installs:?}"
        );
        let project = fs::canonicalize(&fake.project).unwrap();
        assert!(
            accepted.starts_with(&format!("{} ", project.display())),
            "{accepted}"
        );
    }

    #[test]
    fn a_plugin_change_invalidates_only_the_commands_cached_for_its_home() {
        let fake = FakeClaude::new();
        let store = TestStore::new("tcode-plugin-cache");
        let commands = vec![ProviderCommand {
            name: "alpha:greet".into(),
            description: None,
            kind: agent::ProviderCommandKind::Skill,
        }];
        let key = |home: Option<PathBuf>| CommandsCacheKey::Native {
            provider: ProviderKind::ClaudeCode,
            home,
        };
        let other_home = key(Some(fake.dir.join("other-home")));
        for home in [key(Some(fake.home.clone())), key(None), other_home.clone()] {
            store.save_commands(&home, &commands).unwrap();
        }
        let cx = &mut TestAppContext::default();
        let state = cx.new_entity(host_with(&fake, &store));
        assert_eq!(
            state.read(|state| state.cached_provider_commands(
                ProviderKind::ClaudeCode,
                None,
                None
            )),
            commands
        );

        let project = Some(fake.project.clone());
        state.dispatch_command(
            cx,
            1,
            Command::RefreshProviderPlugins {
                profile_id: "claude".into(),
                cwd: project.clone(),
            },
        );
        cx.run_until(settled);
        state.dispatch_command(
            cx,
            2,
            Command::SetProviderPluginEnabled {
                profile_id: "claude".into(),
                entry_id: "alpha@tcode-probe".into(),
                scope: PluginScope::User,
                enabled: false,
                cwd: project,
            },
        );
        cx.run_until(|state| settled(state) && fake.calls("plugin disable").len() == 1);

        assert!(
            state
                .read(|state| state.cached_provider_commands(ProviderKind::ClaudeCode, None, None))
                .is_empty()
        );
        assert_eq!(store.load_commands(&key(None)), commands);
        assert_eq!(store.load_commands(&other_home), commands);
    }

    /// A `claude` per account directory answering the two-account recording
    /// (`accounts/` in the Claude plugin fixtures), and logging each run as
    /// `<account> <cwd> <argv…>`.
    struct FakeAccounts {
        dir: PathBuf,
        binary: PathBuf,
        log: PathBuf,
        homes: PathBuf,
        project: PathBuf,
    }

    impl FakeAccounts {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let dir =
                std::env::temp_dir().join(format!("tcode-fake-accounts-{}", uuid::Uuid::new_v4()));
            let homes = dir.join("homes");
            let project = dir.join("project");
            for path in [homes.join("x"), homes.join("y"), project.clone()] {
                fs::create_dir_all(path).unwrap();
            }
            let project = fs::canonicalize(&project).unwrap();
            let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../agent/tests/fixtures/plugins/claude");
            let log = dir.join("argv.log");
            let binary = dir.join("claude");
            fs::write(
                &binary,
                format!(
                    "#!/bin/sh\n\
                     F='{fixtures}'\n\
                     A=$(basename \"$CLAUDE_CONFIG_DIR\")\n\
                     printf '%s %s %s\\n' \"$A\" \"$(pwd -P)\" \"$*\" >> '{log}'\n\
                     if [ \"$(pwd -P)\" = '{project}' ]; then C=project; else C=outside; fi\n\
                     case \"$*\" in\n\
                     'plugin list --json --available') sed 's#@PROJECT@#{project}#g' \"$F/accounts/$A-list-$C.json\" ;;\n\
                     'plugin marketplace list --json') cat \"$F/accounts/$A-marketplace-list.json\" ;;\n\
                     'plugin details alpha@tcode-probe') cat \"$F/accounts/details-alpha.txt\" ;;\n\
                     'plugin details '*) echo \"$3\" ;;\n\
                     'plugin enable '*) cat \"$F/enable.json\" ;;\n\
                     'plugin disable '*) cat \"$F/disable.json\" ;;\n\
                     *) echo \"unexpected: $*\" >&2; exit 2 ;;\n\
                     esac\n",
                    fixtures = fixtures.display(),
                    log = log.display(),
                    project = project.display(),
                ),
            )
            .unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                dir,
                binary,
                log,
                homes,
                project,
            }
        }

        fn calls(&self) -> Vec<String> {
            fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    impl Drop for FakeAccounts {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Two Claude Code accounts, `x` and `y`, each in its own directory.
    fn two_accounts(fake: &FakeAccounts, store: &TestStore) -> TestClientState {
        let mut state = TestClientState::new((**store).clone());
        for account in ["x", "y"] {
            state.settings.profiles.insert(
                account.into(),
                ProviderProfile {
                    kind: ProviderKind::ClaudeCode,
                    settings: ProviderSettings {
                        binary_path: Some(fake.binary.clone()),
                        home_path: Some(fake.homes.join(account)),
                        ..ProviderSettings::default()
                    },
                },
            );
        }
        state
    }

    fn listed(
        state: &TestClientState,
        profile: &str,
        project: bool,
    ) -> Option<ProviderPluginCatalog> {
        state
            .provider_plugin_catalogs()
            .into_iter()
            .find(|catalog| {
                catalog.profile_id == profile
                    && catalog.context_cwd.is_some() == project
                    && !catalog.loading
                    && catalog.pending.is_empty()
                    && matches!(catalog.state, PluginCatalogState::Fresh)
            })
    }

    fn entry<'a>(catalog: &'a ProviderPluginCatalog, id: &str) -> &'a agent::ProviderPluginEntry {
        catalog.entries.iter().find(|entry| entry.id == id).unwrap()
    }

    /// Each account is listed twice, from outside any project for its own
    /// scope and from the project for the scopes every account shares; a
    /// switch in one account's column runs in that account's directory only,
    /// and a change to the project's scope is seen by every account.
    #[test]
    fn each_account_keeps_its_own_scope_and_shares_the_projects() {
        let fake = FakeAccounts::new();
        let store = TestStore::new("tcode-plugin-accounts");
        let cx = &mut TestAppContext::default();
        let state = cx.new_entity(two_accounts(&fake, &store));
        let project = Some(fake.project.clone());
        let mut id = 0;
        let mut send = |state: &TestEntity, cx: &mut TestAppContext, command: Command| {
            id += 1;
            state.dispatch_command(cx, id, command);
        };
        for account in ["x", "y"] {
            for cwd in [None, project.clone()] {
                send(
                    &state,
                    cx,
                    Command::RefreshProviderPlugins {
                        profile_id: account.into(),
                        cwd,
                    },
                );
            }
        }
        cx.run_until(|state| {
            ["x", "y"].iter().all(|account| {
                listed(state, account, false).is_some() && listed(state, account, true).is_some()
            })
        });

        let (x, y, x_project, y_project) = state.read(|state| {
            (
                listed(state, "x", false).unwrap(),
                listed(state, "y", false).unwrap(),
                listed(state, "x", true).unwrap(),
                listed(state, "y", true).unwrap(),
            )
        });
        let user = PluginScope::User;
        let alpha = "alpha@tcode-probe";
        assert_eq!(entry(&x, alpha).installations[0].scope_enabled, Some(true));
        assert_eq!(entry(&y, alpha).installations[0].scope_enabled, Some(false));
        assert!(
            entry(&x, alpha)
                .actions
                .contains(&PluginAction::Disable { scope: user })
        );
        assert!(
            entry(&y, alpha)
                .actions
                .contains(&PluginAction::Enable { scope: user })
        );
        assert_eq!(entry(&x, alpha).always_on.as_deref(), Some("~5 tok"));
        // Listed from outside, x's install of beta belongs to another
        // project than none; from the project it is the project's own.
        let beta = "beta@tcode-probe";
        let outside = &entry(&x, beta).installations[0];
        assert_eq!(
            (outside.scope, outside.other_project),
            (PluginScope::Project, true)
        );
        let inside = &entry(&x_project, beta).installations[0];
        assert_eq!(
            (inside.scope, inside.other_project, inside.scope_enabled),
            (PluginScope::Project, false, Some(true))
        );
        // y never installed it: its listing of the project has nothing for
        // it but an install.
        assert!(entry(&y_project, beta).installations.is_empty());
        assert!(
            entry(&y_project, beta)
                .actions
                .contains(&PluginAction::Install {
                    scope: PluginScope::Project
                })
        );

        let before = fake.calls().len();
        send(
            &state,
            cx,
            Command::SetProviderPluginEnabled {
                profile_id: "y".into(),
                entry_id: alpha.into(),
                scope: user,
                enabled: true,
                cwd: None,
            },
        );
        cx.run_until(|state| {
            listed(state, "y", false).is_some()
                && fake
                    .calls()
                    .iter()
                    .any(|call| call.contains("plugin enable"))
        });
        let enables: Vec<String> = fake.calls()[before..]
            .iter()
            .filter(|call| {
                !call.contains("plugin list")
                    && !call.contains("plugin marketplace")
                    && !call.contains("plugin details")
            })
            .cloned()
            .collect();
        assert_eq!(enables.len(), 1, "{enables:?}");
        assert!(enables[0].starts_with("y "), "{enables:?}");
        assert!(
            enables[0].ends_with("plugin enable alpha@tcode-probe --scope user --json"),
            "{enables:?}"
        );
        assert!(
            !enables[0].contains(&fake.project.display().to_string()),
            "{enables:?}"
        );

        let project_lists = |account: &str| {
            let prefix = format!("{account} {} plugin list", fake.project.display());
            fake.calls()
                .iter()
                .filter(|call| call.starts_with(&prefix))
                .count()
        };
        let y_before = project_lists("y");
        send(
            &state,
            cx,
            Command::SetProviderPluginEnabled {
                profile_id: "x".into(),
                entry_id: beta.into(),
                scope: PluginScope::Project,
                enabled: false,
                cwd: project.clone(),
            },
        );
        cx.run_until(|state| {
            project_lists("y") > y_before
                && listed(state, "x", true).is_some()
                && listed(state, "y", true).is_some()
        });
        let disables: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|call| call.contains("plugin disable"))
            .collect();
        assert_eq!(
            disables,
            [format!(
                "x {} plugin disable beta@tcode-probe --scope project --json",
                fake.project.display()
            )]
        );
    }
}
