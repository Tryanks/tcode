use super::*;
use std::sync::{Mutex, Weak};
use tcode_core::settings::{EnvVar, ProviderSettings};

pub(super) type ProviderLaunchFuture =
    Pin<Box<dyn Future<Output = Result<SessionHandle, AgentError>> + Send + 'static>>;

#[derive(Clone)]
pub struct ProviderLauncher(
    pub(super) Arc<dyn Fn(ProviderKind, SessionOptions) -> ProviderLaunchFuture + Send + Sync>,
);

impl Default for ProviderLauncher {
    fn default() -> Self {
        Self(Arc::new(|provider, options| {
            Box::pin(agent::start_session(provider, options))
        }))
    }
}

pub(super) fn resolve_selection(
    settings: &Settings,
    provider: ProviderKind,
    profile_id: Option<&str>,
    acp_agent_id: Option<&str>,
) -> Result<ResolvedProfile, ProtocolError> {
    let id = profile_id.unwrap_or_else(|| Settings::builtin_profile_id(provider));
    let profile = settings.resolved_profile(id).ok_or_else(|| {
        error(
            "unknown_profile",
            "This account was deleted. Choose an account before sending.",
        )
    })?;
    if profile.kind != provider {
        return Err(error(
            "invalid_profile",
            "This account belongs to a different provider.",
        ));
    }
    if !profile.settings.enabled {
        return Err(error(
            "provider_disabled",
            "This account is disabled. Enable it in Harness settings before sending.",
        ));
    }
    if provider == ProviderKind::Acp {
        let agent = acp_agent_id
            .and_then(|id| settings.acp_agent(id))
            .ok_or_else(|| {
                error(
                    "unknown_acp_agent",
                    "This ACP agent is no longer installed.",
                )
            })?;
        if !agent.enabled {
            return Err(error(
                "provider_disabled",
                "This ACP agent is disabled. Enable it before sending.",
            ));
        }
    }
    Ok(profile)
}

fn error(code: &str, message: &str) -> ProtocolError {
    ProtocolError {
        code: code.into(),
        message: message.into(),
    }
}

#[derive(Clone, PartialEq)]
struct LaunchConfiguration {
    binary: Option<PathBuf>,
    home: Option<PathBuf>,
    args: Option<String>,
    env: Vec<EnvVar>,
    shared_env: Vec<EnvVar>,
    trust_project_extensions: bool,
    acp: Option<(agent::AcpLaunch, Vec<EnvVar>, Option<String>)>,
}

impl LaunchConfiguration {
    fn new(profile: ResolvedProfile, settings: &Settings, acp_id: Option<&str>) -> Self {
        let ProviderSettings {
            binary_path,
            home_path,
            launch_args,
            env,
            pi,
            ..
        } = profile.settings;
        Self {
            binary: binary_path,
            home: home_path,
            args: launch_args,
            env,
            shared_env: profile.shared_env,
            trust_project_extensions: pi.trust_project_extensions,
            acp: acp_id.and_then(|id| settings.acp_agent(id)).map(|agent| {
                (
                    agent.launch.clone(),
                    agent.env.clone(),
                    agent.launch_args.clone(),
                )
            }),
        }
    }
}

struct ExecutionState {
    provider: ProviderKind,
    profile_id: Option<String>,
    acp_agent_id: Option<String>,
    configuration: LaunchConfiguration,
    availability: Result<(), ProtocolError>,
    configuration_changed: bool,
}

impl ExecutionState {
    fn allowed(&self) -> Result<(), ProtocolError> {
        self.availability.clone()?;
        if self.configuration_changed {
            return Err(error(
                "provider_configuration_changed",
                "The account configuration changed. Waiting for the provider to restart.",
            ));
        }
        Ok(())
    }
}

/// Registered executions include starts still preparing asynchronously and title tasks.
/// Settings updates and channel submissions hold the same lock, so a checked input
/// cannot cross a settings change before it enters the adapter's command channel.
#[derive(Default)]
pub(super) struct ProviderExecutions {
    executions: Vec<Weak<Mutex<ExecutionState>>>,
}

impl ProviderExecutions {
    pub(super) fn prepare(
        &mut self,
        settings: &Settings,
        meta: &SessionMeta,
    ) -> Result<PreparedExecution, ProtocolError> {
        let profile = resolve_selection(
            settings,
            meta.provider,
            meta.profile_id.as_deref(),
            meta.acp_agent_id.as_deref(),
        )?;
        let state = Arc::new(Mutex::new(ExecutionState {
            provider: meta.provider,
            profile_id: meta.profile_id.clone(),
            acp_agent_id: meta.acp_agent_id.clone(),
            configuration: LaunchConfiguration::new(
                profile,
                settings,
                meta.acp_agent_id.as_deref(),
            ),
            availability: Ok(()),
            configuration_changed: false,
        }));
        self.executions
            .retain(|execution| execution.strong_count() > 0);
        self.executions.push(Arc::downgrade(&state));
        Ok(PreparedExecution { state })
    }

    pub(super) fn settings_changed(&mut self, settings: &Settings) {
        self.executions.retain(|execution| {
            let Some(execution) = execution.upgrade() else {
                return false;
            };
            let mut state = execution.lock().unwrap();
            // A new account reusing a deleted account's id does not own its live process.
            if state.availability.as_ref().is_err_and(|error| {
                matches!(error.code.as_str(), "unknown_profile" | "unknown_acp_agent")
            }) {
                return true;
            }
            match resolve_selection(
                settings,
                state.provider,
                state.profile_id.as_deref(),
                state.acp_agent_id.as_deref(),
            ) {
                Ok(profile) => {
                    state.configuration_changed |= state.configuration
                        != LaunchConfiguration::new(
                            profile,
                            settings,
                            state.acp_agent_id.as_deref(),
                        );
                    state.availability = Ok(());
                }
                Err(error) => state.availability = Err(error),
            }
            true
        });
    }

    pub(super) fn secret_changed(&mut self, id: &str) {
        self.executions.retain(|execution| {
            let Some(execution) = execution.upgrade() else {
                return false;
            };
            let mut state = execution.lock().unwrap();
            let profile = state
                .profile_id
                .as_deref()
                .unwrap_or_else(|| Settings::builtin_profile_id(state.provider));
            if id == profile
                || id == Settings::harness_secret_id(state.provider)
                || state
                    .acp_agent_id
                    .as_deref()
                    .is_some_and(|agent| id == Settings::acp_secret_id(agent))
            {
                state.configuration_changed = true;
            }
            true
        });
    }
}

#[derive(Clone)]
pub(super) struct PreparedExecution {
    state: Arc<Mutex<ExecutionState>>,
}

pub(super) struct ProviderHandle {
    pub(super) commands: ProviderCommands,
    pub(super) events: smol::channel::Receiver<AgentEvent>,
}

impl PreparedExecution {
    pub(super) fn allowed(&self) -> Result<(), ProtocolError> {
        self.state.lock().unwrap().allowed()
    }

    pub(super) async fn launch(
        &self,
        launcher: ProviderLauncher,
        options: SessionOptions,
    ) -> Result<ProviderHandle, AgentError> {
        let provider = {
            let state = self.state.lock().unwrap();
            state
                .allowed()
                .map_err(|error| AgentError::Spawn(error.message))?;
            state.provider
        };
        let handle = (launcher.0)(provider, options).await?;
        Ok(ProviderHandle {
            commands: self.connect(handle.commands),
            events: handle.events,
        })
    }

    pub(super) fn connect(
        &self,
        commands: smol::channel::Sender<SessionCommand>,
    ) -> ProviderCommands {
        ProviderCommands {
            commands,
            execution: self.clone(),
        }
    }
}

#[derive(Clone)]
pub(super) struct ProviderCommands {
    commands: smol::channel::Sender<SessionCommand>,
    execution: PreparedExecution,
}

impl ProviderCommands {
    pub(super) fn input_allowed(&self) -> Result<(), ProtocolError> {
        self.execution.allowed()
    }

    pub(super) fn configuration_changed(&self) -> bool {
        self.execution.state.lock().unwrap().configuration_changed
    }

    pub(super) fn try_send(&self, command: SessionCommand) -> Result<(), ProtocolError> {
        let state = self.execution.state.lock().unwrap();
        if command_uses_model(&command) {
            state.allowed()?;
        }
        self.commands
            .try_send(command)
            .map_err(|error| ProtocolError {
                code: "provider_unavailable".into(),
                message: error.to_string(),
            })
    }

    pub(super) fn is_closed(&self) -> bool {
        self.commands.is_closed()
    }
    pub(super) fn same_channel(&self, other: &Self) -> bool {
        self.commands.same_channel(&other.commands)
    }
}

pub(super) fn command_uses_model(command: &SessionCommand) -> bool {
    match command {
        SessionCommand::SendTurn { .. }
        | SessionCommand::Steer { .. }
        | SessionCommand::RetryTurn { .. }
        | SessionCommand::Compact => true,
        SessionCommand::SetGoal { objective, status } => {
            objective.is_some() || *status == Some(agent::GoalStatus::Active)
        }
        SessionCommand::Interrupt
        | SessionCommand::RespondApproval { .. }
        | SessionCommand::RespondUserInput { .. }
        | SessionCommand::SetOption { .. }
        | SessionCommand::StopBackgroundTask { .. }
        | SessionCommand::MoveTo { .. }
        | SessionCommand::RespondWorktree { .. }
        | SessionCommand::MoveToBackground { .. }
        | SessionCommand::ApplySettings { .. }
        | SessionCommand::Rewind { .. }
        | SessionCommand::PreviewRewind { .. }
        | SessionCommand::UndoRewind { .. }
        | SessionCommand::ClearGoal
        | SessionCommand::ReadAccountUsage { .. }
        | SessionCommand::Shutdown => false,
    }
}
