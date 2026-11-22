use super::test_support::*;

use super::*;

use super::{active_session::*, events::*, orchestrate::*, providers::*};

use tcode_core::notice::CHILD_REPORT_REMINDER;

use tcode_core::project::ChildExpectation;

use tcode_core::settings::{SettingsPatch, ThemeMode};

use tcode_protocol::{BackgroundTaskStatus, Command, CommandResponse, HostMessage, Subscription};

/// What crosses the wire for `thread`'s index entry from now on, in order.
fn published_activity(
    cx: &mut TestAppContext,
    thread: &str,
) -> Vec<Option<tcode_core::session::LatestActivity>> {
    cx.drain_outgoing()
        .into_iter()
        .filter_map(|message| match message {
            HostMessage::Event(envelope) => match envelope.event {
                ServerEvent::IndexActivitiesChanged { mut upserts, .. } => upserts
                    .remove(thread)
                    .map(|activity| activity.latest_activity),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Whether the client holds a record of `id`'s log that `matches`.
fn has_record(view: &ClientView, id: &str, matches: impl Fn(&AgentEvent) -> bool) -> bool {
    view.records(id).iter().any(|record| matches(&record.event))
}

fn turn_completed(turn_id: &str, status: TurnStatus) -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: turn_id.into(),
        status,
        usage: None,
    }
}

fn is_turn_end(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::TurnCompleted { .. })
}

/// How the mirror's single turn ends: the Subagent item reaching a terminal
/// status, or the parent process closing while it is still running. A parent
/// `TurnCompleted` is never an ending — a background subagent outlives it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MirrorEnd {
    SubagentCompleted,
    /// Parent turn completes first (background subagent), then a child item
    /// arrives, then the Subagent item completes.
    ParentTurnThenSubagentCompleted,
    SessionClosed,
}

/// A lead's provider runs a subagent of its own: its mirror holds one turn,
/// from the spawn to its end, however the mirror leaves and comes back into
/// the host's memory, whatever arrives after the end, and whether the lead's
/// own turn ends first.
fn assert_native_mirror_turn_lifecycle(
    cx: &mut TestAppContext,
    state: &TestEntity,
    providers: &Providers,
    launch: usize,
    evict: bool,
    late: bool,
    end: MirrorEnd,
) {
    let parent = native_mirror_parent(evict, late, end);
    let parent = parent.as_str();
    let provider = first_turn(state, cx, providers, parent, launch);
    provider.play([native_mirror_parent_item(ItemStatus::InProgress)]);
    let mirror_of = |view: &ClientView| {
        view.sessions()
            .find(|meta| {
                meta.native_subagent.is_some() && meta.parent_session_id.as_deref() == Some(parent)
            })
            .map(|meta| meta.id.clone())
    };
    cx.run_until_seen(|view| mirror_of(view).is_some());
    let mirror_id = mirror_of(cx.seen()).unwrap();
    for i in 0..3 {
        if evict && i == 1 {
            // The user looked at the subagent's thread and went on to
            // others; the host lets it go from memory once more recent ones
            // are left too.
            state.open(cx, &mirror_id);
            cx.run_until_seen(|view| view.timeline(&mirror_id).entries.len() == 1);
            cx.clock.advance(Duration::from_secs(1));
            state.leave(cx, &mirror_id);
            for filler in NATIVE_MIRROR_FILLERS {
                cx.clock.advance(Duration::from_secs(1));
                state.open(cx, filler);
                state.leave(cx, filler);
            }
        }
        provider.play([native_mirror_child_item(i)]);
    }
    match end {
        MirrorEnd::SubagentCompleted => {
            provider.play([native_mirror_parent_item(ItemStatus::Completed)]);
        }
        MirrorEnd::ParentTurnThenSubagentCompleted => {
            provider.play([turn_completed("turn", TurnStatus::Completed)]);
            cx.run_until_seen(|view| has_record(view, parent, is_turn_end));
            if !evict {
                assert!(cx.seen().activity(&mirror_id).unwrap().turn_running);
            }
            provider.play([
                native_mirror_child_item(3),
                native_mirror_parent_item(ItemStatus::Completed),
            ]);
        }
        MirrorEnd::SessionClosed => {
            provider.play([AgentEvent::SessionClosed { reason: None }]);
        }
    }
    if late {
        provider.play([native_mirror_child_item(4)]);
    }
    if end == MirrorEnd::SessionClosed {
        cx.run_until_seen(|view| {
            has_record(view, parent, |event| {
                matches!(event, AgentEvent::SessionClosed { .. })
            })
        });
    } else {
        provider.fence(cx, parent);
    }

    let expected_items =
        3 + usize::from(end == MirrorEnd::ParentTurnThenSubagentCompleted) + usize::from(late);
    let expected_status = if end == MirrorEnd::SessionClosed {
        TurnStatus::Interrupted
    } else {
        TurnStatus::Completed
    };
    state.open(cx, &mirror_id);
    let (mut open, mut boundaries, mut items) = (false, 0, 0);
    for stored in cx.seen().records(&mirror_id) {
        match &stored.event {
            AgentEvent::TurnStarted { .. } => {
                assert!(!open);
                open = true;
                boundaries += 1;
            }
            AgentEvent::TurnCompleted { status, .. } => {
                assert!(open);
                assert_eq!(*status, expected_status);
                open = false;
                boundaries += 1;
            }
            AgentEvent::ItemCompleted(_) => items += 1,
            _ => {}
        }
    }
    assert!(!open, "persisted mirror boundaries must balance");
    assert_eq!(boundaries, 2, "exactly one turn, never a zero-length one");
    assert_eq!(items, expected_items);
    let timeline = cx.seen().timeline(&mirror_id);
    assert!(!timeline.turn_running);
    assert_eq!(timeline.turns.len(), 1);
    let turn = &timeline.turns[0];
    assert!(!turn.running && turn.start_ts.is_some() && turn.end_ts.is_some());
    assert_eq!(turn.status, Some(expected_status));
    assert!(!cx.seen().status(&mirror_id).activity.working);
    state.leave(cx, &mirror_id);
}

/// Threads only the user visits, enough of them that leaving them all lets
/// a thread left before them go from the host's memory.
const NATIVE_MIRROR_FILLERS: [&str; MAX_IDLE_REPLAY_RESIDENTS] =
    ["visited-1", "visited-2", "visited-3", "visited-4"];

fn native_mirror_parent(evict: bool, late: bool, end: MirrorEnd) -> String {
    format!("parent-{evict}-{late}-{end:?}")
}

fn native_mirror_parent_item(status: ItemStatus) -> AgentEvent {
    let item = ThreadItem {
        id: "spawn-1".into(),
        parent_item_id: None,
        content: ItemContent::Subagent {
            agent_type: "explorer".into(),
            description: "Inspect routing".into(),
            status,
            summary: None,
            model: None,
            effort: None,
        },
    };
    if status == ItemStatus::InProgress {
        AgentEvent::ItemStarted(item)
    } else {
        AgentEvent::ItemCompleted(item)
    }
}

fn native_mirror_child_item(i: usize) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: format!("child-{i}"),
        parent_item_id: Some("spawn-1".into()),
        content: ItemContent::AssistantMessage {
            text: format!("answer {i}"),
            summarized: false,
        },
    })
}

fn subagent_item(
    id: &str,
    parent_item_id: Option<&str>,
    agent_type: &str,
    description: &str,
    status: ItemStatus,
) -> ThreadItem {
    ThreadItem {
        id: id.into(),
        parent_item_id: parent_item_id.map(str::to_owned),
        content: ItemContent::Subagent {
            agent_type: agent_type.into(),
            description: description.into(),
            status,
            summary: None,
            model: None,
            effort: None,
        },
    }
}

/// A lead whose turn runs, and the subagents it sent their brief to through
/// `tcode_control`, each running the turn that brief opened and shown on the
/// client. Every thread was stored before the host started, the subagents
/// under the lead, each working in a directory of its own.
struct Fleet {
    state: TestEntity,
    providers: Providers,
    lead: Played,
    children: HashMap<String, Played>,
    root: PathBuf,
}

impl Fleet {
    fn new(
        cx: &mut TestAppContext,
        store: &SessionStore,
        provider: ProviderKind,
        children: &[&str],
        edit: impl Fn(&mut SessionMeta),
    ) -> Self {
        let root = store.root().clone();
        stored_thread(store, "parent", provider, &root.join("parent"), &edit);
        for child in children {
            stored_thread(store, child, provider, &root.join(child), |meta| {
                meta.parent_session_id = Some("parent".into());
                edit(meta);
            });
        }
        let (state, providers) = controlled_host(cx, store);
        state.subscribe(cx, Topic::Index);
        let lead = first_turn(&state, cx, &providers, "parent", 0);
        let mut fleet = Self {
            state,
            providers,
            lead,
            children: HashMap::new(),
            root,
        };
        if !children.is_empty() {
            let sent = fleet.dispatch(cx, children, "Do your part");
            for child in children {
                fleet.state.open(cx, child);
                let played = fleet.providers.started_in(cx, &fleet.root.join(child));
                played.start_turn(cx, "turn-1");
                cx.run_until_seen(|view| view.status(child).running_turn.is_some());
                fleet.children.insert(child.to_string(), played);
            }
            assert!(
                sent["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|result| result["outcome"] == "applied"),
                "{sent}"
            );
        }
        fleet
    }

    fn child(&self, id: &str) -> &Played {
        &self.children[id]
    }

    /// The lead sends `children` `text` after their running turn.
    fn dispatch(
        &self,
        cx: &mut TestAppContext,
        children: &[&str],
        text: &str,
    ) -> serde_json::Value {
        self.lead
            .control(
                cx,
                "send_message",
                serde_json::json!({"thread_ids": children, "text": text, "mode": "queue"}),
            )
            .unwrap()
    }

    /// What the lead is told of its subagents, steered into its running
    /// turn, until the host has taken everything `children` said so far.
    fn callbacks(&self, cx: &mut TestAppContext, children: &[&str]) -> Vec<String> {
        for child in children {
            self.child(child).fence(cx, child);
        }
        cx.run_until_parked();
        std::iter::from_fn(|| next_command(&self.lead.commands).ok())
            .map(|command| match command {
                SessionCommand::Steer { text, .. } => text,
                other => panic!("the lead was sent {other:?}"),
            })
            .collect()
    }

    /// The subagent's report as the index gives it.
    fn report(&self, cx: &TestAppContext, child: &str) -> tcode_core::settlement::AgentReport {
        cx.seen().activity(child).unwrap().agent.unwrap().report
    }
}

/// The child's `report_result` call as its transcript records it.
fn report_result_event(id: &str, text: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: ItemContent::ToolCall {
            image_reads: Vec::new(),
            name: "mcp__tcode_control__report_result".into(),
            input: serde_json::json!({ "text": text }),
            output: Some("{\"ok\":true}".into()),
            status: ItemStatus::Completed,
            family: None,
        },
    })
}

fn completed_turn(turn_id: &str) -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: turn_id.into(),
        status: TurnStatus::Completed,
        usage: None,
    }
}

const HANDED: &str = "handed-over";

fn native(session: &str) -> agent::ResumeCursor {
    agent::ResumeCursor(serde_json::json!({ "session_id": session }))
}

/// The option the thread had chosen while it ran in Codex.
fn codex_effort() -> agent::OptionSelection {
    agent::OptionSelection {
        id: "effort".into(),
        value: "high".into(),
    }
}

/// A thread that ran two turns in Codex with that option and was handed to Claude Code, which
/// ran a third: each harness reported its session and a restore point per
/// prompt, as it does.
fn handed_over(cx: &mut TestAppContext, test_store: &TestStore) -> (TestEntity, Providers) {
    stored_thread(
        test_store,
        HANDED,
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |meta| {
            meta.model = Some("gpt-5".into());
            meta.option_selections = vec![codex_effort()];
        },
    );
    let (state, providers) = played_host(cx, test_store);
    state.open(cx, HANDED);
    let turn = |cx: &mut TestAppContext, played: &Played, name: &str| {
        played.start_turn(cx, &format!("{name}-turn"));
        played.play([
            AgentEvent::TurnCheckpoint {
                turn_id: format!("{name}-turn"),
                checkpoint_id: name.into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("{name}-answer"),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: format!("answer in {name}"),
                    summarized: false,
                },
            }),
            completed_turn(&format!("{name}-turn")),
        ]);
        played.fence(cx, HANDED);
    };
    let started = |played: &Played, session: &str| {
        played.play([AgentEvent::SessionStarted {
            provider_session_id: session.into(),
            resume: native(session),
            model: None,
        }]);
    };
    state.send(cx, HANDED, "first");
    let codex = providers.started(cx, 0);
    started(&codex, "codex-native");
    turn(cx, &codex, "codex-1");
    state.send(cx, HANDED, "second");
    turn(cx, &codex, "codex-2");
    state
        .command(
            cx,
            Command::SetActiveModel {
                session_id: HANDED.into(),
                provider: ProviderKind::ClaudeCode,
                model: Some("claude-opus-5-5".into()),
                profile_id: None,
            },
        )
        .unwrap();
    state
        .command(
            cx,
            Command::ConfirmRelayAndSend {
                session_id: HANDED.into(),
                text: "third".into(),
                attachment_paths: Vec::new(),
            },
        )
        .unwrap();
    let claude = providers.started(cx, 1);
    assert_eq!(claude.provider, ProviderKind::ClaudeCode);
    started(&claude, "claude-native");
    turn(cx, &claude, "claude-1");
    (state, providers)
}

fn rewind_handed(state: &TestEntity, cx: &mut TestAppContext, turn: usize) {
    let command = serde_json::from_value(serde_json::json!({
        "type": "rewind_turn",
        "content": {
            "session_id": HANDED, "turn": turn, "mode": "conversation", "client_id": "desk",
        }
    }))
    .unwrap();
    state.command(cx, command).unwrap();
}

/// The harness and model the thread is on, as its status says.
fn backend(cx: &TestAppContext) -> (ProviderKind, Option<String>) {
    let status = cx.seen().status(HANDED);
    (status.provider, status.requested_model.clone())
}

fn has_relay(cx: &TestAppContext) -> bool {
    cx.seen()
        .timeline(HANDED)
        .entries
        .iter()
        .any(|entry| matches!(entry.content, EntryContent::ProviderRelay { .. }))
}

/// One provider process a test launched: the options it started with, and its command and
/// event channels.
type Launch = (
    SessionOptions,
    smol::channel::Receiver<SessionCommand>,
    smol::channel::Sender<AgentEvent>,
);

type Launches = Arc<std::sync::Mutex<Vec<Launch>>>;

/// A launcher that records every start and gives each its own channels.
fn recording_launcher() -> (ProviderLauncher, Launches) {
    let launches = Arc::new(std::sync::Mutex::new(Vec::<Launch>::new()));
    let recorded = launches.clone();
    let launcher = ProviderLauncher(Arc::new(move |provider, options: SessionOptions| {
        let (commands, command_rx) = smol::channel::unbounded();
        let (event_tx, events) = smol::channel::unbounded();
        recorded
            .lock()
            .unwrap()
            .push((options, command_rx, event_tx));
        Box::pin(async move {
            Ok(SessionHandle {
                provider,
                commands,
                events,
                started_anew: false,
            })
        })
    }));
    (launcher, launches)
}

/// Run `command` through the pipe as a client would, and return its acknowledgement.
fn acked(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    command: Command,
) -> Result<CommandResponse, tcode_protocol::ProtocolError> {
    through_pipe(
        state,
        cx,
        id,
        tcode_protocol::ClientPayload::Command(command),
    )
    .into_iter()
    .find_map(|message| match message {
        HostMessage::Ack { id: acked, result } if acked == id => Some(result),
        _ => None,
    })
    .unwrap()
}

/// Wait for the provider of `launch` to be sent a turn, answering the usage reads before it
/// as [`next_command`] does; the turn's delivery id, text and the channel its events go back
/// on.
fn next_turn(
    cx: &mut TestAppContext,
    launches: &Launches,
    launch: usize,
) -> (u64, String, smol::channel::Sender<AgentEvent>) {
    loop {
        cx.run_until(|_| {
            launches
                .lock()
                .unwrap()
                .get(launch)
                .is_some_and(|launch| !launch.1.is_empty())
        });
        let launches = launches.lock().unwrap();
        let (_, commands, events) = &launches[launch];
        match next_command(commands) {
            Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) => return (delivery_id, text, events.clone()),
            Err(smol::channel::TryRecvError::Empty) => {}
            other => panic!("expected SendTurn, got {other:?}"),
        }
    }
}

/// The next command the provider of `launch` is sent that is not a turn.
fn next_worktree_answer(
    cx: &mut TestAppContext,
    launches: &Launches,
    launch: usize,
) -> (String, agent::WorktreeAnswer) {
    loop {
        cx.run_until(|_| !launches.lock().unwrap()[launch].1.is_empty());
        match next_command(&launches.lock().unwrap()[launch].1) {
            Ok(SessionCommand::RespondWorktree { request_id, answer }) => {
                return (request_id, answer);
            }
            Err(smol::channel::TryRecvError::Empty) => {}
            other => panic!("expected RespondWorktree, got {other:?}"),
        }
    }
}

/// Two Claude accounts, each signed in through its own config directory, an
/// Anthropic-compatible endpoint profile, and a thread whose first turn ran on the built-in
/// account under the native session `native-1`.
struct AccountSwitch {
    _store: TestStore,
    state: TestEntity,
    launches: Launches,
    session_id: String,
    default_home: PathBuf,
    work_home: PathBuf,
}

/// The project directory the CLI keeps the thread's sessions under: its own encoding of the
/// working directory, which Tcode reuses rather than derives.
const CLI_PROJECT_DIR: &str = "-tmp-release-project-3f2a";

impl AccountSwitch {
    /// With `on_disk`, the built-in account's home holds the session as the CLI writes it:
    /// the transcript, the subagent transcripts and stored tool results beside it, its file
    /// checkpoints, task list, todo lists, hook environment and the plans of the slug its
    /// transcript records, next to another session of the same project with its own.
    fn new(cx: &mut TestAppContext, name: &str, on_disk: bool) -> Self {
        let store = TestStore::new(name);
        let cwd = store.root().join("release");
        let default_home = store.root().join("claude-default");
        let work_home = store.root().join("claude-work");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = Project::from_root(cwd.clone());
        store.upsert_project(&project).unwrap();
        let (launcher, launches) = recording_launcher();
        // No `claude` runs here: a usage read the scripted provider leaves unanswered
        // would otherwise start the CLI.
        let binary = Some(store.root().join("no-claude"));
        let mut host = TestClientState::new((*store).clone());
        host.set_provider_launcher(launcher);
        let builtin = host.settings.provider_mut(ProviderKind::ClaudeCode);
        builtin.home_path = Some(default_home.clone());
        builtin.binary_path = binary.clone();
        for (id, home, env) in [
            ("work", &work_home, Vec::new()),
            (
                "proxy",
                &store.root().join("claude-proxy"),
                vec![EnvVar {
                    name: "ANTHROPIC_BASE_URL".into(),
                    value: "https://llm-proxy.example/anthropic".into(),
                    sensitive: false,
                }],
            ),
        ] {
            host.settings.profiles.insert(
                id.into(),
                ProviderProfile {
                    kind: ProviderKind::ClaudeCode,
                    settings: ProviderSettings {
                        display_name: Some(id.into()),
                        env,
                        home_path: Some(home.clone()),
                        binary_path: binary.clone(),
                        ..ProviderSettings::default()
                    },
                },
            );
        }
        let state = cx.new_entity(host);

        let Ok(CommandResponse::SessionId(Some(session_id))) = acked(
            &state,
            cx,
            1,
            Command::StartDraft {
                project_id: project.id.clone(),
                cwd,
            },
        ) else {
            panic!("draft not started");
        };
        let switch = Self {
            _store: store,
            state,
            launches,
            session_id,
            default_home,
            work_home,
        };
        switch.select(cx, 2, None).unwrap();
        switch.send(cx, 3, "plan the release").unwrap();
        let (delivery_id, _, events) = next_turn(cx, &switch.launches, 0);
        if on_disk {
            let project = Path::new("projects").join(CLI_PROJECT_DIR);
            for (path, content) in [
                (
                    project.join("native-1.jsonl"),
                    "{\"turn\":1}\n{\"turn\":1,\"slug\":\"quiet-release-plan\"}\n",
                ),
                (
                    project.join("native-1/subagents/agent-a.jsonl"),
                    "{\"agent\":\"a\"}\n",
                ),
                (project.join("native-1/subagents/agent-a.meta.json"), "{}"),
                (
                    project.join("native-1/tool-results/toolu_1.txt"),
                    "release notes",
                ),
                (
                    project.join("other-session.jsonl"),
                    "{\"other\":true,\"slug\":\"other-plan\"}\n",
                ),
                ("file-history/native-1/9c1e@v1".into(), "before"),
                ("tasks/native-1/1.json".into(), "{\"subject\":\"Tag v1\"}"),
                (
                    "session-env/native-1/sessionstart-hook-0.sh".into(),
                    "export STAGE=release\n",
                ),
                ("todos/native-1-agent-native-1.json".into(), "[]"),
                ("todos/native-1-agent-a.json".into(), "[]"),
                ("plans/quiet-release-plan.md".into(), "# Release"),
                ("plans/quiet-release-plan-agent-a.md".into(), "# Notes"),
                ("tasks/other-session/1.json".into(), "{}"),
                ("todos/other-session-agent-other-session.json".into(), "[]"),
                ("plans/other-plan.md".into(), "# Other"),
            ] {
                let path = switch.default_home.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            }
        }
        switch.run_turn(cx, &events, delivery_id, "Tag v1 on Friday.");
        switch
    }

    fn select(
        &self,
        cx: &mut TestAppContext,
        id: u64,
        profile_id: Option<&str>,
    ) -> Result<CommandResponse, tcode_protocol::ProtocolError> {
        acked(
            &self.state,
            cx,
            id,
            Command::SetActiveModel {
                session_id: self.session_id.clone(),
                provider: ProviderKind::ClaudeCode,
                model: None,
                profile_id: profile_id.map(str::to_owned),
            },
        )
    }

    fn send(
        &self,
        cx: &mut TestAppContext,
        id: u64,
        text: &str,
    ) -> Result<CommandResponse, tcode_protocol::ProtocolError> {
        acked(
            &self.state,
            cx,
            id,
            Command::SendTurn {
                session_id: self.session_id.clone(),
                text: text.into(),
                attachment_paths: Vec::new(),
            },
        )
    }

    /// Answer the turn the provider was sent, as Claude Code does on `native-1`.
    fn run_turn(
        &self,
        cx: &mut TestAppContext,
        events: &smol::channel::Sender<AgentEvent>,
        delivery_id: u64,
        answer: &str,
    ) {
        for event in [
            AgentEvent::SessionStarted {
                provider_session_id: "native-1".into(),
                resume: agent::ResumeCursor(serde_json::json!({ "session_id": "native-1" })),
                model: None,
            },
            AgentEvent::TurnAccepted { delivery_id },
            AgentEvent::TurnStarted {
                turn_id: format!("turn-{delivery_id}"),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("assistant-{delivery_id}"),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: answer.into(),
                    summarized: false,
                },
            }),
            AgentEvent::TurnCompleted {
                turn_id: format!("turn-{delivery_id}"),
                status: TurnStatus::Completed,
                usage: None,
            },
        ] {
            events.try_send(event).unwrap();
        }
        let session_id = self.session_id.clone();
        cx.run_until(|state| {
            state
                .resident(&session_id)
                .is_some_and(|session| !session.turn_in_flight && session.queue.is_empty())
        });
    }

    /// The config directory and resume cursor launch `launch` started with.
    fn launched(&self, launch: usize) -> (Option<PathBuf>, Option<agent::ResumeCursor>) {
        let launches = self.launches.lock().unwrap();
        let options = &launches[launch].0;
        (options.launch_env.home.clone(), options.resume.clone())
    }

    fn relays(&self) -> Vec<Option<String>> {
        self.state.read(|state| {
            state
                .resident(&self.session_id)
                .unwrap()
                .timeline
                .entries
                .iter()
                .filter_map(|entry| match &entry.content {
                    EntryContent::ProviderRelay { prompt, .. } => Some(prompt.clone()),
                    _ => None,
                })
                .collect()
        })
    }
}

fn native_session() -> Option<agent::ResumeCursor> {
    Some(agent::ResumeCursor(
        serde_json::json!({ "session_id": "native-1" }),
    ))
}

/// A Claude Code thread whose harness runs `live` and whose turn has ended,
/// with `background` pieces of work going on, the user then choosing
/// `chosen` for its next message.
fn settings_scene(
    cx: &mut TestAppContext,
    store: &SessionStore,
    id: &str,
    background: usize,
    chosen: &str,
) -> (TestEntity, Providers, Played) {
    stored_thread(
        store,
        id,
        ProviderKind::ClaudeCode,
        &store.root().join(id),
        |meta| {
            meta.model = Some("claude-opus-4-8".into());
        },
    );
    let (state, providers) = played_host(cx, store);
    let provider = first_turn(&state, cx, &providers, id, 0);
    provider.play([
        AgentEvent::BackgroundTasksChanged { count: background },
        completed_turn("turn"),
    ]);
    cx.run_until_seen(|view| view.status(id).running_turn.is_none());
    state
        .command(
            cx,
            Command::SetActiveModel {
                session_id: id.into(),
                provider: ProviderKind::ClaudeCode,
                model: Some(chosen.into()),
                profile_id: None,
            },
        )
        .unwrap();
    (state, providers, provider)
}

/// Whether a lead's agents hold it: one still runs, or a dispatched one
/// finished and awaits its settle.
fn children_hold(activity: &tcode_protocol::SessionActivity) -> bool {
    activity.background.agents > 0 || activity.unsettled > 0
}

/// A stored thread whose working directory exists, so a terminal can start in
/// it.
fn stored_terminal_thread(store: &SessionStore) -> String {
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let meta = SessionMeta::new(ProviderKind::Codex, cwd, None);
    store.upsert_meta(&meta).unwrap();
    meta.id
}

fn session_status_topic(session_id: &str) -> Subscription {
    Subscription {
        known_index: None,
        topic: Topic::SessionStatus {
            session_id: session_id.to_owned(),
        },
        after: None,
    }
}

/// Show the conversation on a client's screen, as client message `id`, and
/// return the status the host answered with.
fn load_conversation(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    session_id: &str,
) -> SessionStatus {
    state.deliver(
        cx,
        id,
        tcode_protocol::ClientPayload::Subscribe(session_status_topic(session_id)),
    );
    cx.run_until_sent(|message| match message {
        HostMessage::Event(EventEnvelope {
            request_id: Some(reply),
            event: ServerEvent::SessionStatusReplaced(status),
            ..
        }) if *reply == id => Some(status.as_ref().clone()),
        _ => None,
    })
    .0
}

fn leave_conversation(state: &TestEntity, cx: &mut TestAppContext, id: u64, session_id: &str) {
    state.deliver(
        cx,
        id,
        tcode_protocol::ClientPayload::Unsubscribe(session_status_topic(session_id)),
    );
    cx.run_until_parked();
}

fn open_terminal_drawer(state: &TestEntity, cx: &mut TestAppContext, id: u64, session_id: &str) {
    state.dispatch_command(
        cx,
        id,
        Command::ToggleTerminalPanel {
            session_id: session_id.to_owned(),
        },
    );
    cx.run_until(|state| {
        state
            .resident(session_id)
            .is_some_and(|thread| !thread.terminal_workspace.terminals.is_empty())
    });
}

fn assert_drawer_closed_without_terminals(state: &TestEntity, status: &SessionStatus) {
    assert!(!status.terminal_open, "the drawer is open");
    assert_eq!(status.terminal_height, 240.);
    assert!(status.terminals.is_empty());
    state.read(|state| {
        assert!(
            !state
                .pending_terminal_spawns
                .contains_key(&status.session_id),
            "a terminal is starting that nobody asked for"
        );
    });
}

fn persisted_assistant_event(text: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: format!("item-{text}"),
        parent_item_id: None,
        content: ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        },
    })
}

/// A repository at `root` on `main` with one commit of `tracked.txt`.
fn init_git_repo(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    run_git(root, &["init", "-b", "main"]).unwrap();
    let config = root.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(
        "[commit]\n\tgpgsign = false\n\
         [core]\n\thooksPath = .no-hooks\n\tautocrlf = false\n\
         [user]\n\tname = tcode\n\temail = tcode@localhost\n",
    );
    std::fs::write(&config, text).unwrap();
    std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
    run_git(root, &["add", "tracked.txt"]).unwrap();
    run_git(root, &["commit", "-m", "initial"]).unwrap();
}

/// Persist `turns` streamed turns for `id` and return the records as the
/// store replays them.
fn persist_streamed_turns(store: &SessionStore, id: &str, turns: u64) -> Vec<StoredEvent> {
    for turn in 0..turns {
        let mut events = vec![
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("user-{turn}"),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: format!("question {turn}"),
                    attachments: Vec::new(),
                },
            }),
            AgentEvent::TurnStarted {
                turn_id: format!("turn-{turn}"),
            },
        ];
        events.extend((0..300).map(|_| AgentEvent::Delta {
            item_id: "pi-assistant-0:0".into(),
            kind: agent::DeltaKind::AssistantText,
            text: "word ".into(),
        }));
        events.push(AgentEvent::TurnCompleted {
            turn_id: format!("turn-{turn}"),
            status: TurnStatus::Completed,
            usage: None,
        });
        let appends: Vec<_> = events
            .iter()
            .enumerate()
            .map(|(offset, event)| {
                tcode_services::store::Mutation::append_event(
                    id,
                    turn * 1000 + offset as u64,
                    event,
                )
                .unwrap()
            })
            .collect();
        store.apply(&appends).unwrap();
    }
    store.read_events(id).unwrap()
}

fn tool_call(id: &str, output: String) -> StoredEvent {
    StoredEvent {
        origin: None,
        author: None,
        ts: Some(1),
        event: AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::ToolCall {
                image_reads: Vec::new(),
                name: "screenshot".into(),
                input: serde_json::json!({}),
                output: Some(output),
                status: ItemStatus::Completed,
                family: None,
            },
        }),
        elided: None,
    }
}

/// A stored turn-changes snapshot: the whole diff of `turn` so far.
fn stored_snapshot(ts: u64, turn: &str, diff: &str) -> String {
    format!(
        "{{\"ts\":{ts},\"event\":{{\"type\":\"turn_changes_updated\",\"turn_id\":\"{turn}\",\
         \"changes\":[{{\"path\":\"f\",\"kind\":\"modify\",\"diff\":{}}}],\
         \"completeness\":\"exact\"}}}}\n",
        serde_json::to_string(&Some(diff).filter(|diff| !diff.is_empty())).unwrap()
    )
}

/// A log as a read of densely numbered rows would load it.
fn session_log(records: impl IntoIterator<Item = StoredEvent>) -> SessionLog {
    let records: Vec<_> = records.into_iter().collect();
    let rows = (0..records.len() as u64).collect();
    SessionLog::new(tcode_services::store::EventLog {
        next_row: records.len() as u64,
        records,
        rows,
    })
}

/// The reply a subscription to a session whose log is resident gets.
fn events_reply(state: &AppState, subscription: &Subscription) -> EventEnvelope {
    let Topic::SessionEvents { session_id } = &subscription.topic else {
        panic!("a subscription to a session's events")
    };
    EventEnvelope {
        request_id: None,
        topic: subscription.topic.clone(),
        event: state.event_records[session_id].events_window(subscription, None),
    }
}

/// Hand `payload` to the host as client message `id`, without running
/// anything it queues.
fn send_to_host(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    payload: tcode_protocol::ClientPayload,
) {
    state.deliver(cx, id, payload);
}

/// Run the host until it answered client message `id` and parked, and
/// return every message it serialized since the last drain.
fn answered(cx: &mut TestAppContext, id: u64) -> Vec<HostMessage> {
    cx.run_until_sent(|message| {
        matches!(message, HostMessage::Ack { id: reply, .. } | HostMessage::QueryResult { id: reply, .. } if *reply == id)
            .then_some(())
    })
    .1
}

/// Send `payload` through the host pipe as client message `id`, and return
/// every message the host serialized since the last drain, once the host
/// answered it and parked.
fn through_pipe(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    payload: tcode_protocol::ClientPayload,
) -> Vec<HostMessage> {
    send_to_host(state, cx, id, payload);
    answered(cx, id)
}

fn subscribe_events(session_id: &str, after: Option<u64>) -> tcode_protocol::ClientPayload {
    tcode_protocol::ClientPayload::Subscribe(Subscription {
        known_index: None,
        topic: Topic::SessionEvents {
            session_id: session_id.into(),
        },
        after,
    })
}

fn unsubscribe_events(session_id: &str) -> tcode_protocol::ClientPayload {
    tcode_protocol::ClientPayload::Unsubscribe(Subscription {
        known_index: None,
        topic: Topic::SessionEvents {
            session_id: session_id.into(),
        },
        after: None,
    })
}

fn history_page(session_id: &str, before: u64) -> tcode_protocol::ClientPayload {
    tcode_protocol::ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage {
        session_id: session_id.into(),
        before,
        limit: 200,
    })
}

/// A session window as the host sent it.
#[derive(Debug, Clone, PartialEq)]
struct Window {
    from: u64,
    end: u64,
    records: Vec<StoredEvent>,
    total: u64,
    total_turns: u64,
}

fn window_of(event: &ServerEvent) -> Option<Window> {
    match event {
        ServerEvent::SessionSnapshot {
            from,
            end,
            records,
            total,
            total_turns,
            ..
        } => Some(Window {
            from: *from,
            end: *end,
            records: records.clone(),
            total: *total,
            total_turns: *total_turns,
        }),
        _ => None,
    }
}

/// The window answering subscription request `id`.
fn window_reply(messages: &[HostMessage], id: u64) -> Window {
    messages
        .iter()
        .find_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                request_id: Some(request),
                event,
                ..
            }) if *request == id => window_of(event),
            _ => None,
        })
        .expect("a window answers the subscription")
}

fn query_reply(messages: &[HostMessage], id: u64) -> QueryResponse {
    messages
        .iter()
        .find_map(|message| match message {
            HostMessage::QueryResult { id: reply, result } if *reply == id => {
                Some(result.clone().expect("query succeeds"))
            }
            _ => None,
        })
        .expect("the query is answered")
}

fn open_stored_session(store: &SessionStore, id: &str) -> SessionMeta {
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, store.root().clone(), None);
    meta.id = id.into();
    store.upsert_meta(&meta).unwrap();
    meta
}

fn next_host_event(events: &async_channel::Receiver<EventEnvelope>) -> EventEnvelope {
    events
        .recv_blocking()
        .expect("the host publishes the lifecycle change before it stops")
}

/// A message sent to a thread whose turn was running when its process went
/// away, with the host that restarted or on its own while the host ran,
/// opens a turn of its own that runs from the provider taking it until the
/// provider ends it, leaving the thread and coming back included; the turn
/// the gone process left ends as interrupted instead of taking the message
/// in, and after a restart it ends when its work stopped.
fn a_message_after_its_process_went_mid_turn_runs_a_turn_of_its_own(restarted: bool) {
    let test_store = TestStore::new("tcode-gone-mid-turn-test");
    let cwd = test_store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, cwd, None);
    meta.id = "thread".into();
    let work = [
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        AgentEvent::ItemStarted(ThreadItem {
            id: "build".into(),
            parent_item_id: None,
            content: ItemContent::CommandExecution {
                command: "make".into(),
                input: Vec::new(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::InProgress,
            },
        }),
    ];
    // The host stopped an hour before the thread was opened again.
    let stopped = test_clock().host_clock().unix_millis();
    test_store.upsert_meta(&meta).unwrap();
    if restarted {
        // A host whose turn was running when it stopped.
        let cx = &mut TestAppContext::default();
        let (state, providers) = played_host(cx, &test_store);
        state.open(cx, "thread");
        state.send(cx, "thread", "Do the task");
        let provider = providers.started(cx, 0);
        provider.start_turn(cx, "turn-1");
        provider.play([work[1].clone()]);
        provider.fence(cx, "thread");
        assert_eq!(cx.now_millis(), stopped);
    }
    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let commands = scripted.commands.clone();
    let mut cx = TestAppContext::default();
    cx.clock.advance(Duration::from_secs(3_600));
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher(scripted.launcher);
        state
    });
    state.update(&mut cx, |state, cx| state.select_session("thread", cx));
    // The reader writes once the thread they reopened is on screen, and a
    // provider's process takes far longer to start than the host takes to
    // load the thread; the scripted one starts at once.
    cx.run_until(|state| {
        !restarted
            || state
                .resident("thread")
                .is_some_and(|session| !session.timeline.turns.is_empty())
    });
    // The host runs the turn the message opened once the provider's events,
    // which reach it off the host, have.
    let send = |cx: &mut TestAppContext, id, text: &str, turn: usize| {
        state.dispatch_command(
            cx,
            id,
            Command::SendTurn {
                session_id: "thread".into(),
                text: text.into(),
                attachment_paths: Vec::new(),
            },
        );
        let SessionCommand::SendTurn { delivery_id, .. } = cx.run_until_command(&commands) else {
            panic!("expected the message to reach the provider")
        };
        // What the Claude Code client emits once the message is written.
        for event in [
            AgentEvent::TurnAccepted { delivery_id },
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
        ] {
            scripted.events.try_send(event).unwrap();
        }
        cx.run_until(|state| {
            state.resident("thread").is_some_and(|session| {
                session.timeline.turns.get(turn).is_some_and(|turn| {
                    turn.running && turn.provider_turn_id.as_deref() == Some("turn-1")
                })
            })
        });
    };
    if !restarted {
        send(&mut cx, 1, "Do the task", 0);
        for event in [
            work[1].clone(),
            AgentEvent::SessionClosed {
                reason: Some("claude exited with exit status: 1".into()),
            },
        ] {
            scripted.events.try_send(event).unwrap();
        }
        cx.run_until(|state| {
            state
                .resident("thread")
                .is_some_and(|session| matches!(session.runtime, Runtime::Idle))
        });
    }
    send(&mut cx, 2, "continue", 1);

    // The turns a client folds from the log, and the one the host runs.
    let turns = |state: &TestEntity| {
        let log = Timeline::fold_stored(&test_store.read_events("thread").unwrap());
        let opening = |index| {
            log.entries
                .iter()
                .find(|entry| entry.turn == index)
                .map(|entry| entry.content.clone())
        };
        let statuses = log.turns.iter().map(|turn| turn.status).collect::<Vec<_>>();
        let running = state.read(|state| {
            state
                .session_status_snapshot("thread")
                .unwrap()
                .running_turn
                .map(|turn| turn.turn)
        });
        (statuses, opening(1), running)
    };
    let continued = Some(EntryContent::Item(ItemContent::UserMessage {
        text: "continue".into(),
        attachments: Vec::new(),
    }));
    let running = (
        vec![Some(TurnStatus::Interrupted), None],
        continued.clone(),
        Some(1),
    );
    assert_eq!(turns(&state), running);
    if restarted {
        let abandoned =
            Timeline::fold_stored(&test_store.read_events("thread").unwrap()).turns[0].end_ts;
        assert_eq!(
            abandoned,
            Some(stopped),
            "the abandoned turn ends when its work stopped, not when it was reopened"
        );
    }
    state.update(&mut cx, |state, cx| state.park_active(cx));
    state.reopen(&mut cx, "thread");
    assert_eq!(turns(&state), running, "coming back ended the turn");

    scripted
        .events
        .try_send(AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: TurnStatus::Completed,
            usage: None,
        })
        .unwrap();
    cx.run_until(|state| {
        state
            .resident("thread")
            .is_some_and(|session| !session.turn_in_flight)
    });
    assert_eq!(
        turns(&state),
        (
            vec![Some(TurnStatus::Interrupted), Some(TurnStatus::Completed)],
            continued,
            None,
        )
    );
}

mod acp_catalog;
mod background;
mod drafts;
mod handoff;
mod history;
mod mirrors;
mod orchestration;
mod providers;
mod queue;
mod settings;
mod settlement;
mod terminals;
mod threads;
mod turns;
mod worktrees;
