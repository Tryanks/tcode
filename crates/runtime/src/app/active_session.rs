use super::*;

/// A message waiting for an ordinary turn. Most are user-authored messages sent
/// while another turn was running; orchestration callbacks also wait here while
/// an idle provider is starting.
///
/// The queue is per-session and in-memory for every provider. Ordinary messages
/// enter the persisted transcript only after the adapter confirms delivery;
/// steering records its own request and acceptance events.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedMessage {
    /// Stable per-session id, so the UI can address a row for steer/drop even
    /// as earlier entries are dispatched out from under it.
    pub id: u64,
    pub(super) delivery_key: Option<String>,
    pub author: Option<Author>,
    /// The space a member sent it from, or whose turn a `tcode_control` call
    /// sent it during.
    pub(super) space: Option<String>,
    /// Held because its space's policy refuses the thread's setup as it
    /// stands ([`AppState::refresh_policy_holds`]).
    pub(super) space_held: bool,
    pub origin: MessageOrigin,
    pub text: String,
    /// Provider-only context for the first turn after a relay. The canonical
    /// user event continues to record only `text`.
    pub(super) relay_transcript: Option<String>,
    pub attachments: Vec<Attachment>,
    /// Earliest wall-clock time at which this turn may dispatch. Scheduled
    /// messages share the in-memory queue and keep a parked provider resident
    /// while pending.
    pub not_before: Option<SystemTime>,
    /// Per-turn settings captured with the user's send gesture. A later mode
    /// toggle must affect later messages, not rewrite work already in the FIFO.
    pub(super) options: TurnOptions,
    /// Context-window selection changed while the provider was live.
    pub(super) context_window_changed: Option<u64>,
    /// Sent on a process that still runs settings the thread has left.
    pub(super) kept_settings: bool,
    /// The children's reports this message carries to the lead.
    pub(super) reports: Vec<ReportHandover>,
    /// Orchestration callbacks arriving during the same provider-start window
    /// are folded into one wake-up turn. Once that turn is live, later callbacks
    /// are steered into it instead of becoming more queued turns.
    pub(super) kind: QueuedMessageKind,
}

/// A child's report callback on its way to the lead: the child's
/// [`tcode_core::project::ChildExpectation::undelivered`] clears once the lead's
/// provider takes this text.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ReportHandover {
    pub(super) child: String,
    pub(super) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum QueuedMessageKind {
    User,
    OrchestrateCallback,
    /// A watched pull request's news, acknowledged on its watch once the provider accepts it.
    PullRequestWake {
        key: tcode_core::pull_request::PullRequestKey,
        id: String,
    },
}

impl QueuedMessage {
    /// The text actually sent to the provider (image-only placeholder and native
    /// skill invocation applied). The recorded user
    /// message keeps `text` verbatim, so an image-only bubble renders as just
    /// its thumbnails.
    pub(super) fn wire_text(
        &self,
        provider: ProviderKind,
        provider_commands: &[ProviderCommand],
    ) -> String {
        let text = if let Some(transcript) = &self.relay_transcript {
            assemble_relay_prompt(transcript, &self.text)
        } else {
            self.text.clone()
        };
        let text = wire_text_with_placeholder(text, &self.attachments, provider);
        native_skill_invocation(text, provider_commands)
    }
}

/// Send a leading `$name` skill mention as `/name` when the provider also lists
/// `name` as a slash command. Claude Code reports every user-invocable skill
/// under both `skills` and `slash_commands`, and expands only the `/name` form
/// (at byte 0 of the message); Codex lists skills alone and expands `$name`
/// itself, so its mentions are left as typed.
pub(super) fn native_skill_invocation(
    text: String,
    provider_commands: &[ProviderCommand],
) -> String {
    let Some(rest) = text.strip_prefix('$') else {
        return text;
    };
    let name_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_len];
    let listed = |kind: ProviderCommandKind| {
        provider_commands
            .iter()
            .any(|command| command.kind == kind && command.name == name)
    };
    if name.is_empty()
        || !listed(ProviderCommandKind::Skill)
        || !listed(ProviderCommandKind::Command)
    {
        return text;
    }
    format!("/{rest}")
}

/// The text of a turn as `provider` is sent it. Claude Code 2.1.322 drops a
/// blank text block before calling the API, and OpenCode 2.2.7 an empty text
/// part, so an image-only message goes to them as the user wrote it. For the
/// others a placeholder stands in for the empty text, and the transcript keeps
/// the user's: Codex's adapter sends the empty `input_text` its own client
/// never sends, and the Responses API's answer to one is unknown; pi passes an
/// empty text part to its OpenAI Responses, Google and Mistral providers; an
/// ACP agent, Cursor and Grok among them, maps the empty block to whatever
/// model API it uses.
pub(super) fn wire_text_with_placeholder(
    text: String,
    attachments: &[Attachment],
    provider: ProviderKind,
) -> String {
    let as_written = matches!(provider, ProviderKind::ClaudeCode | ProviderKind::OpenCode);
    if !as_written && text.trim().is_empty() && !attachments.is_empty() {
        tcode_core::attachments::IMAGE_ONLY_MESSAGE.to_string()
    } else {
        text
    }
}

/// The local persisted paths behind `attachments`, for the recorded event.
pub(super) fn attachment_paths(attachments: &[Attachment]) -> Vec<String> {
    attachments
        .iter()
        .filter_map(|attachment| attachment.source_path.clone())
        .collect()
}

/// What a send gesture resolves to. Enter always means [`Self::Send`] or
/// [`Self::Queue`]; ⌘/Ctrl+Enter additionally reaches [`Self::Steer`] — or
/// [`Self::QueueUnsupported`] when the provider has no steering mechanism, in
/// which case the message is still delivered (queued), just not mid-turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SendRouting {
    /// No turn is running: dispatch immediately as an ordinary turn.
    Send,
    /// A turn is running: hold this message until it completes.
    Queue,
    /// A turn is running and the provider can take a mid-turn injection.
    Steer,
    /// A steer was asked for, but this provider cannot steer. Queue it and tell
    /// the user honestly rather than silently dropping the gesture.
    QueueUnsupported,
}

/// Provider process state for the active session.
pub(super) enum Runtime {
    /// Not started yet — stored session opened (replay only) or brand new.
    Idle,
    /// `start_session` is in flight; queued turns flush when it completes.
    Starting { generation: u64 },
    /// Live child process.
    Live(smol::channel::Sender<SessionCommand>),
}

pub struct ActiveSession {
    pub meta: SessionMeta,
    pub timeline: Timeline,
    /// Git branch of the session cwd, if it is a git repo (display-only).
    pub git_branch: Option<String>,
    /// Local branches for the checkout-row picker, loaded lazily when the
    /// popover opens (empty until then / when not a git repo).
    pub branches: Vec<String>,
    /// A draft thread: set up (provider/model/cwd) but not yet persisted or
    /// started. Materialized into a real session on the first send.
    pub draft: bool,
    pub(super) draft_device_id: Option<String>,
    /// The provider/model that owns the current native history while the picker
    /// previews a different provider. Consumed only by a confirmed send.
    pub(super) pending_relay: Option<PendingRelay>,
    pub(super) runtime: Runtime,
    /// The processes of the provider [`Self::runtime`] starts or runs, which
    /// stopping it kills whatever the provider is waiting on.
    pub(super) provider_processes: agent::SessionProcesses,
    /// A rewind holds the queue until the user next sends a message, which
    /// goes ahead of it.
    pub(super) queue_held: bool,
    /// The model the live provider process actually runs. When the user picks
    /// a different model we compare against this to decide whether it has to
    /// change before the next turn.
    pub(super) live_model: Option<String>,
    /// The directory the live provider process runs in; a move to another directory
    /// sends it there or restarts it before the next turn.
    pub(super) live_cwd: PathBuf,
    /// The profile the live provider process was started with; a switch to
    /// another account restarts it before the next turn.
    pub(super) live_profile_id: Option<String>,
    pub(super) live_option_selections: Vec<OptionSelection>,
    pub(super) confirmed_option_selections: Vec<OptionSelection>,
    /// Sent to the live process, which has yet to answer. Dispatch waits: the
    /// message after a change is the one that asked for it.
    pub(super) settings_apply_in_flight: Option<LiveSettings>,
    /// What the live process last refused to take where it stands; only a
    /// restart brings these.
    pub(super) settings_refused: Option<LiveSettings>,
    /// Draft-only: run in the current checkout or a new dedicated
    /// worktree. Chosen in the checkout row before the first send; locked after.
    pub draft_workspace: WorkspaceMode,
    /// Set while the first send is creating a worktree in the
    /// background (drives the composer's "Preparing worktree…" action).
    pub(super) preparing_worktree: bool,
    /// Messages typed while a turn was running (Enter → queue). In-memory only,
    /// per session — see [`QueuedMessage`].
    pub(super) queue: Vec<QueuedMessage>,
    /// Source of [`QueuedMessage::id`]s.
    pub(super) next_queue_id: u64,
    /// Id of the submitted queue entry awaiting adapter delivery confirmation.
    /// The entry remains in `queue` until acceptance; scheduled rows may precede it.
    pub(super) delivery_in_flight: Option<u64>,
    pub(super) pending_steers: Vec<PendingSteer>,
    /// The space whose policy the latest turn runs under: the sender's space
    /// for a person's message (`None` for the machine's own user), or what the
    /// turn before had for one the host or an agent wrote. A member steering
    /// a turn puts it under their space too.
    pub(super) turn_space: Option<String>,
    /// `turn_space`'s policy as it stood when the turn started, which the
    /// turn keeps however the space changes meanwhile.
    pub(super) turn_policy: Option<tcode_protocol::SpacePolicy>,
    pub(super) turn_in_flight: bool,
    /// The running turn's only input so far is a `tcode_control` watch callback. Such a turn
    /// fires no `turn_completed` or `idle` watch, so two threads watching each other cannot
    /// wake each other forever.
    pub(super) turn_woken_by_control: bool,
    /// Set once an interrupt reached the provider; cleared when the turn ends.
    pub(super) interrupt_requested: bool,
    /// When the first interrupt of that wait went out (unix ms).
    pub(super) interrupt_requested_at: Option<u64>,
    /// A provider command (compaction, a goal change) asked for while the
    /// provider was starting; sent once it is live.
    pub(super) requested_command: Option<SessionCommand>,
    /// Provider-owned background tasks which outlive a completed model turn.
    /// Claude currently supplies this transient liveness signal.
    pub(super) background_task_count: usize,
    /// The work behind that count, for a harness that lists it.
    pub(super) background: super::background::BackgroundWork,
    /// Cleared when the live provider took none of Tcode's MCP servers.
    pub(super) mcp_servers_attached: bool,
    /// When this resident last went fully idle: the end of its live provider's
    /// last turn or background task, otherwise when it was parked. The idle
    /// grace runs from here whether the thread is viewed or parked; working
    /// sessions keep this clear.
    pub(super) idle_since: Option<Instant>,
    /// Provider-native commands / skills discovered at session start (Claude
    /// `slash_commands` + `skills`; Codex `skills/list`).
    /// Seeded from the per-provider cache, then replaced by live updates.
    pub(super) provider_commands: Vec<ProviderCommand>,
    pub(super) provider_options: Vec<OptionDescriptor>,
    /// Lazily-spawned per-session PTYs and provider-bound terminal context.
    pub terminal_workspace: TerminalWorkspace,
    pub(super) _pump: Option<HostTask<()>>,
}

/// A `tcode_control` watch callback, as `fire_control_watches` words it.
fn is_control_callback(text: &str) -> bool {
    text.starts_with(tcode_core::notice::CONTROL_CALLBACK_PREFIX)
}

/// A steer handed to the provider whose consumption it has not yet reported.
/// Kept so a rejected steer can be re-sent as an ordinary turn with the text
/// and admission the user gave it.
#[derive(Debug, Clone)]
pub(super) struct PendingSteer {
    pub(super) request_id: String,
    pub(super) text: String,
    pub(super) attachments: Vec<Attachment>,
    pub(super) author: Option<Author>,
    pub(super) space: Option<String>,
    pub(super) origin: Option<MessageOrigin>,
    pub(super) reports: Vec<ReportHandover>,
}

/// The settings a live process can be asked to take without a restart.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct LiveSettings {
    pub(super) model: Option<String>,
    pub(super) option_selections: Vec<OptionSelection>,
}

#[derive(Debug, Clone)]
pub(super) struct PendingRelay {
    pub(super) from_provider: ProviderKind,
    pub(super) from_model: Option<String>,
    /// The provider profile the history was produced against (`None` = the
    /// built-in profile). Two profiles of one [`ProviderKind`] are distinct
    /// backends with isolated homes, so a profile switch is a relay too,
    /// unless both are accounts of a provider that carries the native session
    /// between them (`SessionMeta::session_transfer_from`).
    pub(super) from_profile: Option<String>,
}

impl ActiveSession {
    pub(super) fn new(
        meta: SessionMeta,
        draft: bool,
        provider_commands: Vec<ProviderCommand>,
    ) -> Self {
        let live_cwd = meta.cwd.clone();
        let live_profile_id = meta.profile_id.clone();
        Self {
            meta,
            timeline: Timeline::default(),
            git_branch: None,
            branches: Vec::new(),
            draft,
            draft_device_id: None,
            pending_relay: None,
            runtime: Runtime::Idle,
            provider_processes: agent::SessionProcesses::default(),
            queue_held: false,
            live_model: None,
            live_cwd,
            live_profile_id,
            live_option_selections: Vec::new(),
            confirmed_option_selections: Vec::new(),
            settings_apply_in_flight: None,
            settings_refused: None,
            draft_workspace: WorkspaceMode::LocalCheckout,
            preparing_worktree: false,
            queue: Vec::new(),
            next_queue_id: 0,
            delivery_in_flight: None,
            pending_steers: Vec::new(),
            turn_space: None,
            turn_policy: None,
            turn_in_flight: false,
            turn_woken_by_control: false,
            interrupt_requested: false,
            interrupt_requested_at: None,
            requested_command: None,
            background_task_count: 0,
            background: Default::default(),
            mcp_servers_attached: true,
            idle_since: None,
            provider_commands,
            provider_options: Vec::new(),
            terminal_workspace: TerminalWorkspace::default(),
            _pump: None,
        }
    }

    pub(super) fn resume_cursor_for_fresh_provider(&mut self) {
        self.shutdown_to_idle();
        self.meta.resume_cursor = None;
        self.meta.pending_fork = None;
        self.meta.session_transfer_from = None;
        self.pending_relay = None;
    }

    /// Whether the live provider is running a different model than the one now
    /// selected in `meta.model`.
    pub(super) fn model_changed_while_live(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_)) && self.meta.model != self.live_model
    }

    pub(super) fn permission_descriptor(&self) -> Option<OptionDescriptor> {
        let descriptor = permission_control(self.meta.provider)?;
        let id = match &descriptor {
            OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => id,
        };
        self.provider_options
            .iter()
            .find(|option| match option {
                OptionDescriptor::Select { id: option_id, .. }
                | OptionDescriptor::Boolean { id: option_id, .. } => option_id == id,
            })
            .cloned()
            .or(Some(descriptor))
    }

    /// The permission selection changed in a way only a restart applies.
    fn permission_restart_pending(&self) -> bool {
        if !matches!(self.runtime, Runtime::Live(_)) {
            return false;
        }
        let permission = self.permission_descriptor();
        let permission_id = permission.as_ref().map(|descriptor| match descriptor {
            OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => {
                id.as_str()
            }
        });
        permission.as_ref().is_some_and(|descriptor| {
            let default = match descriptor {
                OptionDescriptor::Select { default_value, .. } => {
                    default_value.clone().map(serde_json::Value::String)
                }
                OptionDescriptor::Boolean { default_value, .. } => {
                    Some(serde_json::Value::Bool(*default_value))
                }
            };
            let value = |selections: &[OptionSelection]| {
                selections
                    .iter()
                    .find(|selection| Some(selection.id.as_str()) == permission_id)
                    .map(|selection| selection.value.clone())
                    .or_else(|| default.clone())
            };
            matches!(
                descriptor,
                OptionDescriptor::Select {
                    apply: ApplyTiming::Restart,
                    ..
                } | OptionDescriptor::Boolean {
                    apply: ApplyTiming::Restart,
                    ..
                }
            ) && value(&self.meta.option_selections) != value(&self.live_option_selections)
        })
    }

    pub(super) fn options_changed_while_live(&self) -> bool {
        if !matches!(self.runtime, Runtime::Live(_)) {
            return false;
        }
        let permission_restart = self.permission_restart_pending();
        if self.meta.provider.caps().options_apply_live {
            return permission_restart;
        }
        let permission = self.permission_descriptor();
        let permission_id = permission.as_ref().map(|descriptor| match descriptor {
            OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => {
                id.as_str()
            }
        });
        let ignore_effort = self.meta.provider.caps().per_turn_effort;
        let model_selections = |selections: &[OptionSelection]| {
            let selections: Vec<_> = selections
                .iter()
                .filter(|selection| Some(selection.id.as_str()) != permission_id)
                .cloned()
                .collect();
            normalized_selections(&selections, ignore_effort)
        };
        permission_restart
            || model_selections(&self.meta.option_selections)
                != model_selections(&self.live_option_selections)
    }

    /// No turn, delivery, queued message or provider background task is pending.
    pub(super) fn fully_idle(&self) -> bool {
        !self.turn_in_flight
            && self.delivery_in_flight.is_none()
            && (self.queue.is_empty() || self.queue_held)
            && self.background_task_count == 0
    }

    pub(super) fn launch_settings_changed_while_live(&self) -> bool {
        self.model_changed_while_live()
            || self.options_changed_while_live()
            || self.cwd_changed_while_live()
            || self.profile_changed_while_live()
    }

    fn selected_settings(&self) -> LiveSettings {
        LiveSettings {
            model: self.meta.model.clone(),
            option_selections: self.meta.option_selections.clone(),
        }
    }

    /// The live process differs from the thread only in what it can be asked
    /// to take where it stands, and has not refused these settings.
    fn settings_apply_due(&self) -> bool {
        self.meta.provider.caps().live_settings
            && (self.model_changed_while_live() || self.options_changed_while_live())
            && !self.cwd_changed_while_live()
            && !self.profile_changed_while_live()
            && !self.permission_restart_pending()
            && self.settings_refused.as_ref() != Some(&self.selected_settings())
    }

    /// The live process cannot run this thread's settings without a restart.
    pub(super) fn restart_required(&self) -> bool {
        self.launch_settings_changed_while_live() && !self.settings_apply_due()
    }

    /// The live process's answer to [`Self::settings_apply_in_flight`].
    pub(super) fn settle_settings_apply(&mut self, applied: bool) {
        let Some(settings) = self.settings_apply_in_flight.take() else {
            return;
        };
        if applied {
            self.live_model = settings.model;
            self.live_option_selections = settings.option_selections;
        } else {
            self.settings_refused = Some(settings);
        }
    }

    /// Give up carrying the native session to the account the thread switched to (the copy
    /// failed, or the provider closed before it resumed the copy): providers start fresh, and
    /// the next message hands them the conversation from the account that holds it.
    pub(super) fn fail_session_transfer(&mut self) {
        self.meta.resume_cursor = None;
        self.meta.pending_fork = None;
    }

    /// Whether the thread's native session stays behind in the profile
    /// `session_transfer_from` names ([`Self::fail_session_transfer`]).
    pub(super) fn session_transfer_failed(&self) -> bool {
        self.meta.session_transfer_from.is_some() && self.meta.resume_cursor.is_none()
    }

    /// Whether the live provider runs under another profile than `meta.profile_id`
    /// with no relay pending: the thread switched accounts and takes its native
    /// session along. A relay keeps the old process until it is confirmed.
    fn profile_changed_while_live(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_))
            && self.pending_relay.is_none()
            && self.live_profile_id != self.meta.profile_id
    }

    /// Whether the live provider runs in another directory than `meta.cwd` and has to
    /// restart to leave it: the thread moved while it was live, and its native session
    /// stays behind.
    pub(super) fn cwd_changed_while_live(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_))
            && self.live_cwd != self.meta.cwd
            && !self.native_move_due()
    }

    /// Whether the live provider is to be sent to `meta.cwd` before its next turn: the
    /// thread moved while it was live and its native session goes along.
    fn native_move_due(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_))
            && self.live_cwd != self.meta.cwd
            && self.meta.provider.caps().session_move
            && self.meta.moved_from.is_none()
    }

    /// A settings restart must not kill a running turn (a `/compact` included),
    /// provider-owned background work, or a turn whose provider delivery
    /// acknowledgement has not landed yet. Only the first and last also hold
    /// the queue: background work runs for as long as it likes, so a message
    /// goes out beside it on the process as it is ([`Self::dispatch_next_pending`]).
    pub(super) fn settings_restart_deferred(&self) -> bool {
        self.restart_required()
            && (self.turn_in_flight
                || self.background_task_count > 0
                || self.delivery_in_flight.is_some())
    }

    /// The queued message the next dispatch would send, if one could go at
    /// `now`, the host's wall time.
    fn next_dispatchable(&self, now: SystemTime) -> Option<&QueuedMessage> {
        if self.queue_held
            || self.turn_in_flight
            || self.delivery_in_flight.is_some()
            || self.settings_apply_in_flight.is_some()
            || !matches!(self.runtime, Runtime::Live(_))
        {
            return None;
        }
        self.queue.iter().find(|message| {
            !message.space_held && message.not_before.is_none_or(|time| time <= now)
        })
    }

    /// The id and text of the message that opens the fresh provider session of a moved
    /// thread, or of one whose native session could not be carried to its new account,
    /// which carries the handoff; `None` until that session is live and a message can go.
    pub(super) fn handoff_message(&self, now: SystemTime) -> Option<(u64, String)> {
        if (self.meta.moved_from.is_none() && !self.session_transfer_failed())
            || self.cwd_changed_while_live()
            || self.profile_changed_while_live()
        {
            return None;
        }
        self.next_dispatchable(now)
            .map(|message| (message.id, message.text.clone()))
    }

    pub(super) fn turn_options(&self) -> TurnOptions {
        let effort = if self.meta.provider.caps().per_turn_effort {
            effort_selection(&self.meta.option_selections)
        } else {
            None
        };
        TurnOptions { effort }
    }

    /// Tear down the live provider and return to `Idle` so the next
    /// `ensure_started` respawns it (with the current model + resume cursor).
    /// Queued sends are preserved so they flush once the new process is up.
    pub(super) fn shutdown_to_idle(&mut self) {
        self.stop_provider();
        self.runtime = Runtime::Idle;
        self.delivery_in_flight = None;
        self.settings_apply_in_flight = None;
        self.turn_in_flight = false;
        self.interrupt_requested = false;
        self.interrupt_requested_at = None;
        self.requested_command = None;
        self.background_task_count = 0;
        self.background.clear();
        self.idle_since = None;
        self._pump = None;
    }

    /// The thread's next turn waits on its provider, which has no turn running
    /// yet: it is starting, still to answer a live settings change, or still to
    /// take the message.
    pub(super) fn preparing_turn(&self) -> bool {
        !self.turn_in_flight
            && !self.timeline.turn_running
            && (matches!(self.runtime, Runtime::Starting { .. })
                || self.settings_apply_in_flight.is_some()
                || self.delivery_in_flight.is_some())
    }

    /// Stop the provider this session starts or runs: its actor is asked, and
    /// its processes are killed, which ends whatever it waits on.
    pub(super) fn stop_provider(&self) {
        if let Runtime::Live(commands) = &self.runtime {
            let _ = commands.try_send(SessionCommand::Shutdown);
        }
        self.provider_processes.stop();
    }

    /// Forget a provider process that has already closed on its own.
    pub(super) fn mark_dead(&mut self) {
        self.runtime = Runtime::Idle;
        self.delivery_in_flight = None;
        self.settings_apply_in_flight = None;
        self.turn_in_flight = false;
        self.interrupt_requested = false;
        self.interrupt_requested_at = None;
        self.requested_command = None;
        self.background_task_count = 0;
        self.background.clear();
        self._pump = None;
    }

    /// Whether a message typed right now could be STEERED into the turn that is
    /// already running — i.e. the provider has a native mid-turn injection
    /// mechanism (Claude: a stream-json user message; Codex: `turn/steer`) and
    /// is actually live. When false, the composer's steer gesture degrades to
    /// queueing (and says so).
    pub(crate) fn can_steer(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_)) && self.meta.provider.caps().supports_steering
    }

    /// Whether this session owns work which must stay live and surface as
    /// "Working", regardless of whether it is active or parked.
    pub(super) fn has_work(&self) -> bool {
        self.turn_in_flight
            || self.delivery_in_flight.is_some()
            || !self.queue.is_empty()
            || self.background_task_count > 0
    }

    /// Whether the queue is all this thread has left: its start failed or its
    /// provider died, so nothing is working on the unsent text, which waits for
    /// the next start. A draft's scheduled send has yet to start at all.
    pub(super) fn queue_stranded(&self) -> bool {
        !self.draft && matches!(self.runtime, Runtime::Idle) && !self.queue.is_empty()
    }

    /// Whether a parent still has to wait for this thread: it has work, or its
    /// provider is still starting up.
    pub(super) fn is_unfinished(&self) -> bool {
        self.has_work() || matches!(self.runtime, Runtime::Starting { .. })
    }

    /// Where a send gesture should go, given what the session is doing right
    /// now. This is the whole steering-vs-queueing policy in one place.
    pub(super) fn route(&self, steer: bool) -> SendRouting {
        if !self.turn_in_flight {
            // Nothing to steer into: ⌘Enter and Enter are the same thing.
            SendRouting::Send
        } else if !steer {
            SendRouting::Queue
        } else if self.can_steer() && self.timeline.turn_retry.is_none() {
            SendRouting::Steer
        } else {
            SendRouting::QueueUnsupported
        }
    }

    /// Pull a message out of the queue by id (the strip's steer/✕ buttons).
    pub(super) fn take_queued(&mut self, id: u64) -> Option<QueuedMessage> {
        if self.delivery_in_flight == Some(id) {
            return None;
        }
        let index = self.queue.iter().position(|m| m.id == id)?;
        Some(self.queue.remove(index))
    }

    /// Put queued message `id` just before `before`, or last when `None`.
    /// Moving a submitted entry is harmless: its acceptance correlates by id.
    pub(super) fn move_queued(&mut self, id: u64, before: Option<u64>) {
        if before == Some(id) {
            return;
        }
        let Some(from) = self.queue.iter().position(|m| m.id == id) else {
            return;
        };
        let message = self.queue.remove(from);
        let to = before
            .and_then(|before| self.queue.iter().position(|m| m.id == before))
            .unwrap_or(self.queue.len());
        self.queue.insert(to, message);
    }

    /// Inject a message into the turn already in flight. Deliberately does NOT
    /// touch the turn bookkeeping: the provider folds the message into the
    /// running turn (Claude emits no second `result`; Codex's `turn/steer`
    /// resolves with the same `turnId`), so `turn_in_flight` stays true and the
    /// queue is untouched. Opening a turn here would leave a phantom that never
    /// completes.
    pub(super) fn steer_now(
        &mut self,
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    ) -> Result<(), ()> {
        let Runtime::Live(commands) = &self.runtime else {
            return Err(());
        };
        if !is_control_callback(&text) {
            self.turn_woken_by_control = false;
        }
        commands
            .try_send(SessionCommand::Steer {
                request_id,
                text,
                attachments,
            })
            .map_err(|_| ())
    }

    /// Append a message to the queue.
    pub(super) fn push_queued(&mut self, text: String, attachments: Vec<Attachment>) -> u64 {
        self.idle_since = None;
        let id = self.next_queue_id;
        self.next_queue_id += 1;
        let options = self.turn_options();
        self.queue.push(QueuedMessage {
            delivery_key: None,
            author: None,
            space: None,
            space_held: false,
            origin: MessageOrigin::Human,
            id,
            text,
            relay_transcript: None,
            attachments,
            not_before: None,
            options,
            context_window_changed: None,
            kept_settings: false,
            reports: Vec::new(),
            kind: QueuedMessageKind::User,
        });
        id
    }

    /// Put a steer the provider never took back in the queue, under its own
    /// author and origin.
    pub(super) fn requeue_steer(&mut self, steer: PendingSteer) {
        self.push_queued(steer.text, steer.attachments);
        if let Some(message) = self.queue.last_mut() {
            message.author = steer.author;
            message.space = steer.space;
            message.origin = steer.origin.unwrap_or(MessageOrigin::Human);
            message.reports = steer.reports;
        }
    }

    /// Append a delayed user turn while capturing the same per-send settings
    /// and annotations as an ordinary queued message.
    pub(super) fn push_scheduled(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        not_before: SystemTime,
    ) -> u64 {
        let id = self.push_queued(text, attachments);
        self.queue.last_mut().unwrap().not_before = Some(not_before);
        id
    }

    /// Keep callbacks that race while an idle provider is starting in the same
    /// wake-up turn. Sending them as separate queued turns lets the first result
    /// drive the orchestrator before the rest are visible, and the leftovers may
    /// not run until much later.
    pub(super) fn push_or_merge_orchestrate_callback(&mut self, text: String) -> u64 {
        self.idle_since = None;
        let delivery_in_flight = self.delivery_in_flight;
        // A merged message must still read as one notice (`tcode_core::notice::host_notice`).
        let control = is_control_callback;
        if let Some(pending) = self.queue.iter_mut().find(|message| {
            message.kind == QueuedMessageKind::OrchestrateCallback
                && Some(message.id) != delivery_in_flight
                && control(&message.text) == control(&text)
        }) {
            pending.text.push_str("\n\n");
            pending.text.push_str(&text);
            return pending.id;
        }
        let id = self.next_queue_id;
        self.next_queue_id += 1;
        let options = self.turn_options();
        self.queue.push(QueuedMessage {
            delivery_key: None,
            author: None,
            space: None,
            space_held: false,
            origin: MessageOrigin::Agent,
            id,
            text,
            relay_transcript: None,
            attachments: Vec::new(),
            not_before: None,
            options,
            context_window_changed: None,
            kept_settings: false,
            reports: Vec::new(),
            kind: QueuedMessageKind::OrchestrateCallback,
        });
        id
    }

    /// Queue a host-authored turn behind any running turn.
    pub(super) fn push_server_message(&mut self, text: String, kind: QueuedMessageKind) -> u64 {
        self.idle_since = None;
        let id = self.next_queue_id;
        self.next_queue_id += 1;
        let options = self.turn_options();
        self.queue.push(QueuedMessage {
            delivery_key: None,
            author: None,
            space: None,
            space_held: false,
            origin: MessageOrigin::Server,
            id,
            text,
            relay_transcript: None,
            attachments: Vec::new(),
            not_before: None,
            options,
            context_window_changed: None,
            kept_settings: false,
            reports: Vec::new(),
            kind,
        });
        id
    }

    /// Stamp the last queued message with the context window it will run at
    /// when that differs from the live provider's; `default_window` is the
    /// model's default preset, which an unselected side runs at.
    pub(super) fn stamp_context_window_change(&mut self, default_window: Option<u64>) {
        if !matches!(self.runtime, Runtime::Live(_)) {
            return;
        }
        let window = |selections: &[OptionSelection]| {
            agent::context_window::selected(&[], selections).or(default_window)
        };
        let selected = window(&self.meta.option_selections);
        if selected != window(&self.live_option_selections)
            && let Some(message) = self.queue.last_mut()
        {
            message.context_window_changed = selected;
        }
    }

    /// Dispatch at most one eligible queued message as an ordinary turn. FIFO
    /// is preserved among eligible entries, while a future scheduled entry may
    /// be passed by ordinary work. A turn already in flight blocks dispatch for EVERY provider: a
    /// queued message is by definition one that waits for the running turn to
    /// finish. (Steering — the other way to send mid-turn — never goes through
    /// here; see [`AppState::steer`].) A restart background work defers does not
    /// block it: the message runs on the live process's settings and says so.
    pub(super) fn dispatch_next_pending(&mut self, now: SystemTime) -> Result<bool, ()> {
        let Some(send) = self.next_dispatchable(now).cloned() else {
            return Ok(false);
        };
        let Runtime::Live(commands) = &self.runtime else {
            return Ok(false);
        };
        if self.settings_apply_due() {
            let settings = self.selected_settings();
            commands
                .try_send(SessionCommand::ApplySettings {
                    model: settings.model.clone(),
                    option_selections: settings.option_selections.clone(),
                })
                .map_err(|_| ())?;
            self.settings_apply_in_flight = Some(settings);
            return Ok(false);
        }
        if self.native_move_due() {
            commands
                .try_send(SessionCommand::MoveTo {
                    cwd: self.meta.cwd.clone(),
                })
                .map_err(|_| ())?;
            self.live_cwd = self.meta.cwd.clone();
        }
        let text = send.wire_text(self.meta.provider, &self.provider_commands);
        commands
            .try_send(SessionCommand::SendTurn {
                delivery_id: send.id,
                text,
                options: Some(send.options),
                attachments: send.attachments,
            })
            .map_err(|_| ())?;
        let kept_settings = self.restart_required();
        if let Some(message) = self.queue.iter_mut().find(|message| message.id == send.id) {
            message.kept_settings = kept_settings;
        }
        self.idle_since = None;
        self.delivery_in_flight = Some(send.id);
        self.turn_space = match send.origin {
            MessageOrigin::Human => send.space,
            MessageOrigin::Agent | MessageOrigin::Server => send.space.or(self.turn_space.take()),
        };
        Ok(true)
    }

    /// Commit exactly one submitted queue entry after its correlated adapter
    /// acceptance. Eligibility-based dispatch can submit a non-head entry, so
    /// id correlation—not position—is authoritative. Duplicate/stale
    /// acknowledgements are harmless and never persist twice.
    pub(super) fn accept_turn_delivery(&mut self, delivery_id: u64) -> Option<QueuedMessage> {
        if self.delivery_in_flight != Some(delivery_id) {
            return None;
        }
        let position = self
            .queue
            .iter()
            .position(|message| message.id == delivery_id)?;
        self.delivery_in_flight = None;
        self.turn_in_flight = true;
        self.idle_since = None;
        let message = self.queue.remove(position);
        self.turn_woken_by_control = message.kind == QueuedMessageKind::OrchestrateCallback
            && is_control_callback(&message.text);
        Some(message)
    }

    pub(super) fn is_starting_generation(&self, generation: u64) -> bool {
        matches!(
            self.runtime,
            Runtime::Starting {
                generation: current
            } if current == generation
        )
    }
}

pub(super) fn conversation_destination(active: &ActiveSession) -> ConversationDestination {
    if active.draft
        && let Some(project_id) = active.meta.project_id.clone()
    {
        ConversationDestination::ProjectDraft(project_id)
    } else {
        ConversationDestination::Thread(active.meta.id.clone())
    }
}
