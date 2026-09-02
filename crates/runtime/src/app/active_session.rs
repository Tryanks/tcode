use super::*;

/// A message waiting for an ordinary turn. Most are user-authored messages sent
/// while another turn was running; orchestration callbacks also wait here while
/// an idle provider is starting.
///
/// Queueing is an APP-LEVEL concept and works for every provider, including the
/// ones that cannot steer. The queue is per-session and in-memory only: it is
/// deliberately NOT persisted to the session JSONL, because a queued message is
/// not yet part of the conversation (it is recorded only once it is actually
/// dispatched, or steered, as a user message).
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedMessage {
    /// Stable per-session id, so the UI can address a row for steer/drop even
    /// as earlier entries are dispatched out from under it.
    pub id: u64,
    pub text: String,
    /// Provider-only context for the first turn after a relay. The canonical
    /// user event continues to record only `text`.
    pub(super) relay_transcript: Option<String>,
    pub attachments: Vec<Attachment>,
    /// Earliest wall-clock time at which this turn may dispatch. Scheduled
    /// messages ride the same in-memory queue as ordinary turns, so v1 keeps a
    /// parked provider resident while one is pending instead of persisting or
    /// rehydrating a separate scheduler record.
    pub not_before: Option<SystemTime>,
    /// Per-turn settings captured with the user's send gesture. A later mode
    /// toggle must affect later messages, not rewrite work already in the FIFO.
    pub(super) options: TurnOptions,
    /// Ultrathink was armed when this message was written. It is a per-send
    /// prompt-prefix mode, so it rides with the message rather than with the
    /// session, and is applied only to the text sent on the wire (the user
    /// message recorded in the transcript stays clean).
    pub(super) ultrathink: bool,
    /// Byte length of an injected context prefix folded into `text` (set only for
    /// an `/orchestrate` send). Threaded into the recorded user-message event so
    /// the timeline can split the prefix from the user's own words; `None` for
    /// every ordinary send.
    pub(super) context_len: Option<usize>,
    /// Context-window selection changed while the provider was live.
    pub(super) context_window_changed: Option<u64>,
    /// Orchestration callbacks arriving during the same provider-start window
    /// are folded into one wake-up turn. Once that turn is live, later callbacks
    /// are steered into it instead of becoming more queued turns.
    pub(super) kind: QueuedMessageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueuedMessageKind {
    User,
    OrchestrateCallback,
}

impl QueuedMessage {
    /// The text actually sent to the provider (image-only placeholder and
    /// Ultrathink prefix applied). The recorded user message keeps `text`
    /// verbatim, so an image-only bubble renders as just its thumbnails.
    pub(super) fn wire_text(&self) -> String {
        let text = if let Some(transcript) = &self.relay_transcript {
            assemble_relay_prompt(transcript, &self.text)
        } else {
            self.text.clone()
        };
        let text = wire_text_with_placeholder(text, &self.attachments);
        if self.ultrathink {
            format!("Ultrathink:\n{text}")
        } else {
            text
        }
    }
}

/// Providers require non-empty turn text: an image-only message goes on the
/// wire with T3's synthetic placeholder while the transcript records the
/// user's (empty) text plus the attachments.
pub(super) fn wire_text_with_placeholder(text: String, attachments: &[Attachment]) -> String {
    if text.trim().is_empty() && !attachments.is_empty() {
        tcode_core::attachments::image_only_message().to_string()
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

impl From<&str> for QueuedMessage {
    fn from(text: &str) -> Self {
        QueuedMessage {
            id: 0,
            text: text.to_string(),
            relay_transcript: None,
            attachments: Vec::new(),
            not_before: None,
            options: TurnOptions::default(),
            ultrathink: false,
            context_len: None,
            context_window_changed: None,
            kind: QueuedMessageKind::User,
        }
    }
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
    /// The provider/model that owns the current native history while the picker
    /// previews a different provider. Consumed only by a confirmed send.
    pub(super) pending_relay: Option<PendingRelay>,
    pub(super) runtime: Runtime,
    /// The model the live provider process was actually started with. When the
    /// user picks a different model we compare against this to decide whether a
    /// restart is needed before the next turn.
    pub(super) live_model: Option<String>,
    /// The approval mode the live provider process is actually running under.
    /// Claude switches live (this is updated in lockstep, so no restart);
    /// Codex binds the mode at thread start, so a mid-session change leaves this
    /// stale and forces a resume-restart before the next turn.
    pub(super) live_approval_mode: Option<ApprovalMode>,
    /// The option selections the live provider was started with (reasoning
    /// effort, context window, fast mode, …). A mid-session change to a
    /// launch-time option forces a resume-restart before the next turn; Codex's
    /// reasoning effort is the exception (it applies per turn, see `send_turn`).
    pub(super) live_option_selections: Vec<OptionSelection>,
    /// A transient "the next send should be an Ultrathink turn" flag, set when
    /// the user picks Ultrathink in the traits picker. It is never persisted
    /// (T3: Ultrathink is a prompt-prefix mode, not an option) and is cleared
    /// after one send.
    pub(super) pending_ultrathink: bool,
    /// A transient "the next queued send carries an injected context prefix of
    /// this many bytes" flag, set by [`AppState::orchestrate_turn`] right before
    /// it hands the composed text to `steer`. Like `pending_ultrathink` it is a
    /// per-send annotation, consumed by the next `push_queued`, and never
    /// persisted on the session.
    pub(super) pending_context_len: Option<usize>,
    /// Draft-only (Group C): run in the current checkout or a new dedicated
    /// worktree. Chosen in the checkout row before the first send; locked after.
    pub draft_workspace: WorkspaceMode,
    /// Group C: set while the first send is creating a worktree in the
    /// background (drives the composer's "Preparing worktree…" action).
    pub(super) preparing_worktree: bool,
    /// Messages typed while a turn was running (Enter → queue). In-memory only,
    /// per session — see [`QueuedMessage`].
    pub(super) queue: Vec<QueuedMessage>,
    /// Source of [`QueuedMessage::id`]s.
    pub(super) next_queue_id: u64,
    /// Queue head submitted to the adapter but not yet confirmed at its native
    /// delivery boundary. The head remains in `queue` until acceptance.
    pub(super) delivery_in_flight: Option<u64>,
    pub(super) turn_in_flight: bool,
    /// Provider-owned background tasks which outlive a completed model turn.
    /// Claude currently supplies this transient liveness signal.
    pub(super) background_task_count: usize,
    /// When this parked, fully idle provider became eligible for grace-period
    /// retention and LRU eviction. Active or working sessions keep this clear.
    pub(super) idle_since: Option<Instant>,
    /// Provider-native commands / skills discovered at session start (Claude
    /// `slash_commands` + `skills`; Codex `skills/list` + custom prompts).
    /// Seeded from the per-provider cache, then replaced by live updates.
    pub(super) provider_commands: Vec<ProviderCommand>,
    /// The agent's self-described options (ACP `modes` / `models` /
    /// `configOptions`), pushed over the wire at session start and on every
    /// change. They render through the composer's existing traits picker; the
    /// native providers describe their options through the model catalog
    /// instead, so this stays empty for them. In-memory only.
    pub(super) provider_options: Vec<OptionDescriptor>,
    /// Lazily-spawned per-session PTYs and provider-bound terminal context.
    pub terminal_workspace: TerminalWorkspace,
    pub(super) _pump: Option<HostTask<()>>,
}

#[derive(Debug, Clone)]
pub(super) struct PendingRelay {
    pub(super) from_provider: ProviderKind,
    pub(super) from_model: Option<String>,
    /// The provider profile the history was produced against (`None` = the
    /// built-in profile). Two profiles of one [`ProviderKind`] are distinct
    /// backends with isolated homes, so a profile switch is a relay too.
    pub(super) from_profile: Option<String>,
}

impl ActiveSession {
    pub(super) fn new(
        meta: SessionMeta,
        draft: bool,
        provider_commands: Vec<ProviderCommand>,
    ) -> Self {
        Self {
            meta,
            timeline: Timeline::default(),
            git_branch: None,
            branches: Vec::new(),
            draft,
            pending_relay: None,
            runtime: Runtime::Idle,
            live_model: None,
            live_approval_mode: None,
            live_option_selections: Vec::new(),
            pending_ultrathink: false,
            pending_context_len: None,
            draft_workspace: WorkspaceMode::LocalCheckout,
            preparing_worktree: false,
            queue: Vec::new(),
            next_queue_id: 0,
            delivery_in_flight: None,
            turn_in_flight: false,
            background_task_count: 0,
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
        self.meta.pending_fork = false;
        self.pending_relay = None;
    }

    /// Whether the live provider is running a different model than the one now
    /// selected in `meta.model` (so the next turn must restart the provider).
    pub(super) fn model_changed_while_live(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_)) && self.meta.model != self.live_model
    }

    /// Whether the live provider is running a different approval mode than the
    /// one now selected in `meta.approval_mode`. Only providers that cannot
    /// switch live (Codex) reach this state: Claude updates `live_approval_mode`
    /// in lockstep when it applies the switch on the wire.
    pub(super) fn approval_mode_changed_while_live(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_))
            && Some(self.meta.approval_mode) != self.live_approval_mode
    }

    /// Whether a launch-time option (reasoning effort for Claude, context
    /// window, fast mode, thinking, …) changed while the provider is live, so
    /// the next turn must restart it. Codex and OpenCode reasoning effort is
    /// excluded: it is applied per turn via [`TurnOptions`] and needs no restart.
    pub(super) fn options_changed_while_live(&self) -> bool {
        if !matches!(self.runtime, Runtime::Live(_)) {
            return false;
        }
        // ACP agents take option changes live (`session/set_mode` /
        // `set_model` / `set_config_option`), so nothing ever needs a restart.
        if self.meta.provider.caps().options_apply_live {
            return false;
        }
        let ignore_effort = self.meta.provider.caps().per_turn_effort;
        normalized_selections(&self.meta.option_selections, ignore_effort)
            != normalized_selections(&self.live_option_selections, ignore_effort)
    }

    pub(super) fn launch_settings_changed_while_live(&self) -> bool {
        self.model_changed_while_live()
            || self.approval_mode_changed_while_live()
            || self.options_changed_while_live()
    }

    /// A settings restart must not kill Claude-owned background work or race a
    /// turn whose provider delivery acknowledgement has not landed yet.
    pub(super) fn settings_restart_deferred(&self) -> bool {
        self.launch_settings_changed_while_live()
            && (self.background_task_count > 0 || self.delivery_in_flight.is_some())
    }

    /// Per-turn overrides derived from the session's persisted state: Codex and
    /// OpenCode reasoning effort, plus the Build/Plan interaction mode.
    pub(super) fn turn_options(&self) -> TurnOptions {
        let effort = if self.meta.provider.caps().per_turn_effort {
            effort_selection(&self.meta.option_selections)
        } else {
            None
        };
        TurnOptions {
            effort,
            interaction_mode: Some(self.meta.interaction_mode),
        }
    }

    /// Tear down the live provider and return to `Idle` so the next
    /// `ensure_started` respawns it (with the current model + resume cursor).
    /// Queued sends are preserved so they flush once the new process is up.
    pub(super) fn shutdown_to_idle(&mut self) {
        if let Runtime::Live(commands) = &self.runtime {
            let _ = commands.try_send(SessionCommand::Shutdown);
        }
        self.runtime = Runtime::Idle;
        self.delivery_in_flight = None;
        self.turn_in_flight = false;
        self.background_task_count = 0;
        self.idle_since = None;
        self._pump = None;
    }

    /// Forget a provider process that has already closed on its own.
    pub(super) fn mark_dead(&mut self) {
        self.runtime = Runtime::Idle;
        self.delivery_in_flight = None;
        self.turn_in_flight = false;
        self.background_task_count = 0;
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

    /// Whether a turn is currently running, i.e. Enter queues rather than sends.
    pub(crate) fn is_turn_running(&self) -> bool {
        self.turn_in_flight
    }

    /// Whether this session owns work which must stay live and surface as
    /// "Working", regardless of whether it is active or parked.
    pub(super) fn has_work(&self) -> bool {
        self.turn_in_flight
            || self.delivery_in_flight.is_some()
            || !self.queue.is_empty()
            || self.background_task_count > 0
    }

    /// Where a send gesture should go, given what the session is doing right
    /// now. This is the whole steering-vs-queueing policy in one place.
    pub(super) fn route(&self, steer: bool) -> SendRouting {
        if !self.is_turn_running() {
            // Nothing to steer into: ⌘Enter and Enter are the same thing.
            SendRouting::Send
        } else if !steer {
            SendRouting::Queue
        } else if self.can_steer() {
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
        commands
            .try_send(SessionCommand::Steer {
                request_id,
                text,
                attachments,
            })
            .map_err(|_| ())
    }

    /// Append a message to the queue, consuming the armed Ultrathink flag (it is
    /// per-send, so it belongs to this message, not to whatever is sent later).
    pub(super) fn push_queued(&mut self, text: String, attachments: Vec<Attachment>) -> u64 {
        self.idle_since = None;
        let id = self.next_queue_id;
        self.next_queue_id += 1;
        let options = self.turn_options();
        let ultrathink = std::mem::take(&mut self.pending_ultrathink);
        let context_len = std::mem::take(&mut self.pending_context_len);
        let context_window_changed = self.context_window_change();
        self.queue.push(QueuedMessage {
            id,
            text,
            relay_transcript: None,
            attachments,
            not_before: None,
            options,
            ultrathink,
            context_len,
            context_window_changed,
            kind: QueuedMessageKind::User,
        });
        id
    }

    /// Append a delayed user turn while capturing the same per-send settings
    /// and annotations as an ordinary queued message.
    pub(super) fn push_scheduled(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        not_before: SystemTime,
    ) -> u64 {
        self.idle_since = None;
        let id = self.next_queue_id;
        self.next_queue_id += 1;
        let options = self.turn_options();
        let ultrathink = std::mem::take(&mut self.pending_ultrathink);
        let context_len = std::mem::take(&mut self.pending_context_len);
        let context_window_changed = self.context_window_change();
        self.queue.push(QueuedMessage {
            id,
            text,
            relay_transcript: None,
            attachments,
            not_before: Some(not_before),
            options,
            ultrathink,
            context_len,
            context_window_changed,
            kind: QueuedMessageKind::User,
        });
        id
    }

    /// Keep callbacks that race while an idle provider is starting in the same
    /// wake-up turn. Sending them as separate queued turns lets the first result
    /// drive the orchestrator before the rest are visible, and the leftovers may
    /// not run until much later.
    pub(super) fn push_or_merge_orchestrate_callback(&mut self, text: String) -> u64 {
        self.idle_since = None;
        let delivery_in_flight = self.delivery_in_flight;
        if let Some(pending) = self.queue.iter_mut().find(|message| {
            message.kind == QueuedMessageKind::OrchestrateCallback
                && Some(message.id) != delivery_in_flight
        }) {
            pending.text.push_str("\n\n");
            pending.text.push_str(&text);
            return pending.id;
        }
        let id = self.next_queue_id;
        self.next_queue_id += 1;
        let options = self.turn_options();
        self.queue.push(QueuedMessage {
            id,
            text,
            relay_transcript: None,
            attachments: Vec::new(),
            not_before: None,
            options,
            ultrathink: false,
            context_len: None,
            context_window_changed: None,
            kind: QueuedMessageKind::OrchestrateCallback,
        });
        id
    }

    fn context_window_change(&self) -> Option<u64> {
        if !matches!(self.runtime, Runtime::Live(_)) {
            return None;
        }
        let model = self.meta.model.as_deref().unwrap_or_default();
        let selected = agent::claude::resolved_context_window(model, &self.meta.option_selections);
        let live = agent::claude::resolved_context_window(model, &self.live_option_selections);
        (selected != live).then_some(selected)
    }

    /// Dispatch at most one eligible queued message as an ordinary turn. FIFO
    /// is preserved among eligible entries, while a future scheduled entry may
    /// be passed by ordinary work. A turn already in flight blocks dispatch for EVERY provider: a
    /// queued message is by definition one that waits for the running turn to
    /// finish. (Steering — the other way to send mid-turn — never goes through
    /// here; see [`AppState::steer`].)
    pub(super) fn dispatch_next_pending(&mut self) -> Result<bool, ()> {
        if self.turn_in_flight
            || self.delivery_in_flight.is_some()
            || self.settings_restart_deferred()
        {
            return Ok(false);
        }
        let Runtime::Live(commands) = &self.runtime else {
            return Ok(false);
        };
        let now = SystemTime::now();
        let Some(send) = self
            .queue
            .iter()
            .find(|message| message.not_before.is_none_or(|time| time <= now))
            .cloned()
        else {
            return Ok(false);
        };
        commands
            .try_send(SessionCommand::SendTurn {
                delivery_id: send.id,
                text: send.wire_text(),
                options: Some(send.options),
                attachments: send.attachments,
            })
            .map_err(|_| ())?;
        self.idle_since = None;
        self.delivery_in_flight = Some(send.id);
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
        Some(self.queue.remove(position))
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
    if active.draft {
        ConversationDestination::ProjectDraft(
            active
                .meta
                .project_id
                .clone()
                .unwrap_or_else(|| active.meta.id.clone()),
        )
    } else {
        ConversationDestination::Thread(active.meta.id.clone())
    }
}
