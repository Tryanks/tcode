use super::ChatView;
use crate::store::WorkspaceStore;
use crate::test_frames::draw_settled;
use crate::window_state::WindowState;
use agent::{ItemContent, ItemStatus};
use gpui::{AppContext as _, Entity, TestAppContext};
use std::sync::{Arc, atomic::AtomicU64};
use tcode_core::session::{EntryContent, Timeline, TimelineEntry, TurnMeta};

pub(in crate::chat) static NEXT_RESIDENCY_TEST_ID: AtomicU64 = AtomicU64::new(0);

#[gpui::test]
fn a_compact_composer_shows_effort_approval_and_context(cx: &mut TestAppContext) {
    use gpui::px;
    let mut timeline = synthetic_markdown_timeline(1);
    timeline.usage = Some(agent::TokenUsage {
        used_tokens: Some(100_000),
        context_window: Some(200_000),
        ..Default::default()
    });
    // A catalog-backed model, as the host caches it: the generic chat
    // fixture has no model options and therefore cannot detect a missing
    // effort control.
    let disk = chat_store(cx);
    disk.save_models(
        agent::ProviderKind::Codex,
        &[agent::ModelSpec {
            id: "effort-layout".into(),
            display_name: "A model with a long display name".into(),
            is_default: true,
            options: vec![agent::OptionDescriptor::Select {
                id: "reasoningEffort".into(),
                label: "Reasoning effort".into(),
                options: vec![agent::SelectOption {
                    value: "xhigh".into(),
                    label: "Extra High".into(),
                    description: None,
                    unavailable: None,
                    speed: None,
                    summary: None,
                    approval: None,
                    apply: None,
                }],
                default_value: Some("xhigh".into()),
                role: agent::OptionRole::Effort,
                apply: Default::default(),
                recommended: None,
                permissive: None,
                minimum: None,
            }],
        }],
    )
    .unwrap();
    let mut meta = thread_meta(None);
    meta.provider = agent::ProviderKind::Codex;
    meta.model = Some("effort-layout".into());
    record_thread(&disk, &meta, log_of(&timeline));
    let session_id = meta.id;
    let host = spawn_chat_host(disk, Default::default());
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    driver.wait_until(cx, "workspace baseline", |cx| {
        store.read_with(cx, |store, _| {
            store.baseline_ready()
                && store
                    .provider_model_catalog(agent::ProviderKind::Codex)
                    .iter()
                    .any(|model| model.id == "effort-layout")
        })
    });
    store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
    driver.wait_until(cx, "selected conversation", |cx| {
        store.read_with(cx, |store, cx| {
            store.baseline_ready() && !store.chat_loading_for(store.sole_thread().as_deref(), cx)
        })
    });
    let window_state = cx.new(|_| WindowState::default().with_compact(true));
    let (_view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
        ChatView::new(
            store.clone(),
            window_state,
            Some(session_id.clone()),
            window,
            cx,
        )
    });
    cx.simulate_resize(gpui::size(px(393.), px(852.)));
    draw_settled(cx);
    for (selector, what) in [
        ("permission-chip", "approval option"),
        ("traits-chip", "standalone effort option"),
        ("context-meter", "compact context meter"),
        ("composer-settings-drawer", "attached settings drawer"),
    ] {
        assert!(cx.debug_bounds(selector).is_some(), "{what}");
    }
    driver.shutdown(cx, &host);
}

/// A window drawing the conversation `build` makes as the panes draw
/// one: through its frame.
pub(crate) fn add_chat_window(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut gpui::Window, &mut gpui::Context<ChatView>) -> ChatView,
) -> (Entity<ChatView>, &mut gpui::VisualTestContext) {
    struct Framed {
        chat: Entity<ChatView>,
        frame: Entity<super::ChatFrame>,
    }
    impl gpui::Render for Framed {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            self.frame.clone()
        }
    }
    let (root, cx) = cx.add_window_view(|window, cx| {
        let chat = cx.new(|cx| build(window, cx));
        let frame = chat.read(cx).frame();
        Framed { chat, frame }
    });
    let chat = root.read_with(cx, |root, _| root.chat.clone());
    (chat, cx)
}

/// [`add_chat_window`] under the root that hosts dialogs, as the app's
/// windows are, for a test whose path opens one.
pub(crate) fn add_chat_window_hosting_dialogs(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut gpui::Window, &mut gpui::Context<ChatView>) -> ChatView,
) -> (Entity<ChatView>, &mut gpui::VisualTestContext) {
    let mut chat = None;
    let (_root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| build(window, cx));
        let frame = view.read(cx).frame();
        chat = Some(view);
        gpui_base::Root::new(frame, window, cx)
    });
    (chat.expect("the chat is built"), cx)
}

pub(crate) fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
    Arc::new(TimelineEntry {
        id: id.to_string(),
        content,
        ts: None,
        finished_ts: None,
        background: false,
        turn: 0,
    })
}

pub(crate) fn user_item(text: &str) -> EntryContent {
    EntryContent::Item(ItemContent::UserMessage {
        text: text.into(),
        attachments: Vec::new(),
    })
}

pub(crate) fn assistant(text: &str) -> EntryContent {
    EntryContent::Item(ItemContent::AssistantMessage {
        text: text.into(),
        summarized: false,
    })
}

pub(in crate::chat) fn reasoning(text: &str) -> EntryContent {
    EntryContent::Item(ItemContent::Reasoning { text: text.into() })
}

pub(in crate::chat) fn command(id: &str) -> Arc<TimelineEntry> {
    entry(
        id,
        EntryContent::Item(ItemContent::CommandExecution {
            command: id.to_string(),
            input: Vec::new(),
            output: String::new(),
            exit_code: Some(0),
            status: agent::ItemStatus::Completed,
        }),
    )
}

pub(crate) fn command_with_output(id: &str) -> Arc<TimelineEntry> {
    entry(
        id,
        EntryContent::Item(ItemContent::CommandExecution {
            command: format!("run {id}"),
            input: Vec::new(),
            output: format!("output of {id}\n"),
            exit_code: Some(0),
            status: ItemStatus::Completed,
        }),
    )
}

/// The output of the command `id`'s open detail, painted in the last
/// frame.
pub(in crate::chat) fn command_open(
    cx: &mut gpui::VisualTestContext,
    id: &str,
) -> Option<gpui::Bounds<gpui::Pixels>> {
    cx.debug_bounds(format!("command-output-{id}").leak())
}

pub(in crate::chat) fn at_turn(mut entry: Arc<TimelineEntry>, turn: usize) -> Arc<TimelineEntry> {
    Arc::make_mut(&mut entry).turn = turn;
    entry
}

pub(in crate::chat) fn synthetic_markdown_timeline(turn_count: usize) -> Timeline {
    let mut timeline = Timeline::default();
    timeline.turns = vec![TurnMeta::default(); turn_count];
    for turn in 0..turn_count {
        timeline.entries.extend([
            at_turn(
                entry(&format!("user-{turn}"), user_item(&format!("User {turn}"))),
                turn,
            ),
            at_turn(
                entry(
                    &format!("reasoning-{turn}"),
                    reasoning(&format!("Reasoning {turn}")),
                ),
                turn,
            ),
            at_turn(
                entry(
                    &format!("assistant-{turn}"),
                    assistant(&format!("Assistant {turn}")),
                ),
                turn,
            ),
        ]);
    }
    timeline
}

pub(in crate::chat) fn single_assistant_timeline(id: &str, text: &str) -> Timeline {
    let mut timeline = Timeline::default();
    timeline.turns = vec![TurnMeta::default()];
    timeline.entries.push(entry(id, assistant(text)));
    timeline
}

pub(in crate::chat) fn seed_chat(
    cx: &mut TestAppContext,
    timeline: Timeline,
) -> (Entity<WorkspaceStore>, Entity<WindowState>, String) {
    let (store, window_state, session_id, _) = seed_chat_with_history(cx, timeline, false);
    (store, window_state, session_id)
}

pub(in crate::chat) fn seeded_chat_host(
    cx: &mut TestAppContext,
    timeline: Timeline,
    paged: bool,
) -> (tcode_runtime::pipe::SpawnedHost, String) {
    seeded_chat_host_on(cx, log_of(&timeline), paged, None, Default::default())
}

/// The log a host holds for `timeline`: each turn started, its entries
/// as the events that record them, and the turn completed unless it
/// still runs.
pub(in crate::chat) fn log_of(timeline: &Timeline) -> Vec<agent::AgentEvent> {
    use agent::AgentEvent;
    let turns = timeline
        .entries
        .iter()
        .map(|entry| entry.turn + 1)
        .max()
        .unwrap_or(0)
        .max(timeline.turns.len());
    let mut log = Vec::new();
    for turn in 0..turns {
        let turn_id = format!("turn-{turn}");
        log.push(AgentEvent::TurnStarted {
            turn_id: turn_id.clone(),
        });
        if let Some(checkpoint_id) = timeline
            .turns
            .get(turn)
            .and_then(|meta| meta.provider_checkpoint_id.clone())
        {
            log.push(AgentEvent::TurnCheckpoint {
                turn_id: turn_id.clone(),
                checkpoint_id,
            });
        }
        for entry in timeline.entries.iter().filter(|entry| entry.turn == turn) {
            match &entry.content {
                EntryContent::Item(content) => {
                    let running = serde_json::to_value(content)
                        .ok()
                        .and_then(|content| content.get("status").cloned())
                        .is_some_and(|status| status == "in_progress");
                    let item = agent::ThreadItem {
                        id: entry.id.clone(),
                        parent_item_id: None,
                        content: content.clone(),
                    };
                    log.push(if running {
                        AgentEvent::ItemStarted(item)
                    } else {
                        AgentEvent::ItemCompleted(item)
                    });
                }
                EntryContent::Steer {
                    text,
                    status,
                    attachments,
                } => {
                    log.push(AgentEvent::SteerRequested {
                        request_id: entry.id.clone(),
                        text: text.clone(),
                        attachments: attachments.clone(),
                    });
                    if *status == tcode_core::session::SteeringStatus::Accepted {
                        log.push(AgentEvent::SteerAccepted {
                            request_id: entry.id.clone(),
                        });
                    }
                }
                other => panic!("no event in this fixture records {other:?}"),
            }
        }
        if !timeline.turns.get(turn).is_some_and(|meta| meta.running) {
            log.push(AgentEvent::TurnCompleted {
                turn_id,
                status: agent::TurnStatus::Completed,
                usage: None,
            });
        }
    }
    if timeline.goal.is_some() {
        log.push(AgentEvent::GoalUpdated {
            goal: timeline.goal.clone(),
        });
    }
    if !timeline.plan_steps.is_empty() {
        log.push(AgentEvent::PlanUpdated {
            turn_id: None,
            explanation: timeline.plan_explanation.clone(),
            steps: timeline.plan_steps.clone(),
        });
    }
    if let Some(usage) = timeline.usage {
        log.push(AgentEvent::TokenUsage(usage));
    }
    log
}

/// A thread on `provider`, with a native session to resume when one is
/// given.
pub(crate) fn thread_meta(
    provider: Option<agent::ProviderKind>,
) -> tcode_core::project::SessionMeta {
    let mut meta = tcode_core::project::SessionMeta::new(
        provider.unwrap_or(agent::ProviderKind::ClaudeCode),
        std::env::temp_dir(),
        None,
    );
    if provider.is_some() {
        meta.resume_cursor = Some(agent::ResumeCursor(
            serde_json::json!({"session_id": "seeded"}),
        ));
    }
    meta
}

/// `meta`'s thread, recorded in `store` with `log`, as a host finds a
/// thread an earlier run recorded.
pub(crate) fn record_thread(
    store: &tcode_services::store::SessionStore,
    meta: &tcode_core::project::SessionMeta,
    log: impl IntoIterator<Item = agent::AgentEvent>,
) {
    store.upsert_meta(meta).expect("record the thread");
    for (ts, event) in log.into_iter().enumerate() {
        store
            .append_event(&meta.id, ts as u64, &event)
            .expect("record the thread's log");
    }
}

/// A thread on a host, shown in `store`, whose turn a scripted provider
/// runs: it has sent the events the thread was given, and goes on
/// running for as long as the test holds `provider`.
pub(crate) struct RunningThread {
    pub(crate) store: Entity<WorkspaceStore>,
    pub(crate) session_id: String,
    pub(crate) provider: ScriptedHandles,
}

/// A stored thread the user sends a turn, whose provider sends `events`;
/// returns once `arrived` says the window has them.
pub(crate) fn running_thread(
    cx: &mut TestAppContext,
    events: Vec<agent::AgentEvent>,
    arrived: impl Fn(&WorkspaceStore, &gpui::App) -> bool,
) -> RunningThread {
    let disk = chat_store(cx);
    let meta = thread_meta(None);
    record_thread(&disk, &meta, []);
    let scripts = tcode_runtime::app::Scripts::default();
    let host = spawn_chat_host(disk, scripts.launcher());
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    driver.wait_until(cx, "workspace baseline", |cx| {
        store.read_with(cx, |store, _| store.baseline_ready())
    });
    store.update(cx, |store, cx| store.show_alone(meta.id.clone(), cx));
    driver.wait_until(cx, "selected conversation", |cx| {
        store.read_with(cx, |store, cx| {
            !store.chat_loading_for(store.sole_thread().as_deref(), cx)
        })
    });
    let provider = scripted_turn(
        &scripts,
        &mut driver,
        &meta.id,
        agent::ProviderKind::ClaudeCode,
        cx,
    );
    for event in events {
        provider.1.try_send(event).unwrap();
    }
    driver.wait_until(cx, "the provider's events", |cx| {
        store.read_with(cx, |store, cx| {
            arrived(store, cx)
                && store
                    .sole_status(cx)
                    .is_some_and(|status| status.activity.turn_running)
        })
    });
    RunningThread {
        store,
        session_id: meta.id,
        provider,
    }
}

/// A host started over a store holding one thread with `log`.
pub(in crate::chat) fn seeded_log_host(
    cx: &mut TestAppContext,
    log: Vec<agent::AgentEvent>,
) -> (tcode_runtime::pipe::SpawnedHost, String) {
    let store = chat_store(cx);
    let meta = thread_meta(None);
    record_thread(&store, &meta, log);
    (spawn_chat_host(store, Default::default()), meta.id)
}

/// The user sends `thread` a turn that a scripted `provider` plays: what
/// the test sends on the returned channel reaches the host as the
/// provider's events, and the commands it gets back are the provider's.
pub(crate) fn scripted_turn(
    scripts: &tcode_runtime::app::Scripts,
    driver: &mut crate::store::tests::LocalHostDriver,
    thread: &str,
    provider: agent::ProviderKind,
    cx: &mut TestAppContext,
) -> ScriptedHandles {
    let scripted = scripts.script(provider);
    let handles = (scripted.commands.clone(), scripted.events.clone());
    driver.command(
        cx,
        tcode_protocol::Command::SendTurn {
            session_id: thread.to_string(),
            text: "Go on.".into(),
            attachment_paths: Vec::new(),
        },
    );
    // The provider takes the turn, as an adapter acknowledges a send.
    loop {
        if let agent::SessionCommand::SendTurn { delivery_id, .. } = driver.recv(cx, &handles.0) {
            handles
                .1
                .try_send(agent::AgentEvent::TurnAccepted { delivery_id })
                .unwrap();
            return handles;
        }
    }
}

/// A store for one test's host, with markdown and the client set up.
pub(crate) fn chat_store(cx: &mut TestAppContext) -> tcode_services::store::SessionStore {
    cx.update(crate::test_client::init);
    cx.update(crate::markdown::init);
    tcode_services::store::SessionStore::open_at(crate::store::tests::scratch_root(
        "chat-residency-test",
    ))
    .expect("test session store")
}

/// A host over `store` that starts providers with `launcher`.
pub(crate) fn spawn_chat_host(
    store: tcode_services::store::SessionStore,
    launcher: tcode_runtime::app::ProviderLauncher,
) -> tcode_runtime::pipe::SpawnedHost {
    tcode_runtime::pipe::spawn_host(
        store,
        tcode_runtime::pipe::HostServices {
            provider_launcher: launcher,
            ..Default::default()
        },
    )
    .expect("spawn test host")
}

/// A host started over a store holding one thread with `log`, on
/// `provider` when given. When `paged`, two thousand warnings come
/// first, so the thread opens on a window of its log.
pub(in crate::chat) fn seeded_chat_host_on(
    cx: &mut TestAppContext,
    log: Vec<agent::AgentEvent>,
    paged: bool,
    provider: Option<agent::ProviderKind>,
    launcher: tcode_runtime::app::ProviderLauncher,
) -> (tcode_runtime::pipe::SpawnedHost, String) {
    let store = chat_store(cx);
    let meta = thread_meta(provider);
    let warnings = (0..if paged { 2000 } else { 0 }).map(|_| agent::AgentEvent::Warning {
        message: "short".into(),
    });
    record_thread(&store, &meta, warnings.chain(log));
    (spawn_chat_host(store, launcher), meta.id)
}

/// Two threads on one host, the first selected and holding `first`, the
/// second holding `second`.
pub(in crate::chat) struct TwoThreads {
    pub(in crate::chat) store: Entity<WorkspaceStore>,
    pub(in crate::chat) driver: crate::store::tests::LocalHostDriver,
    pub(in crate::chat) host: tcode_runtime::pipe::SpawnedHost,
    pub(in crate::chat) scripts: tcode_runtime::app::Scripts,
    pub(in crate::chat) first: String,
    pub(in crate::chat) second: String,
}

pub(in crate::chat) fn seed_two_threads(
    cx: &mut TestAppContext,
    first: Timeline,
    second: Timeline,
) -> TwoThreads {
    let store = chat_store(cx);
    let (first_meta, second_meta) = (thread_meta(None), thread_meta(None));
    record_thread(&store, &first_meta, log_of(&first));
    record_thread(&store, &second_meta, log_of(&second));
    let scripts = tcode_runtime::app::Scripts::default();
    let host = spawn_chat_host(store, scripts.launcher());
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    driver.wait_until(cx, "workspace baseline", |cx| {
        store.read_with(cx, |store, _| store.baseline_ready())
    });
    store.update(cx, |store, cx| store.show_alone(first_meta.id.clone(), cx));
    driver.wait_until(cx, "chat baseline", |cx| {
        store.read_with(cx, |store, cx| {
            !store.chat_loading_for(store.sole_thread().as_deref(), cx)
        })
    });
    TwoThreads {
        store,
        driver,
        host,
        scripts,
        first: first_meta.id,
        second: second_meta.id,
    }
}

impl TwoThreads {
    /// Binds `chat`'s pane to `thread`, as the shell does, once the
    /// thread's history has arrived.
    pub(in crate::chat) fn bind(
        &mut self,
        chat: &Entity<ChatView>,
        thread: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        let store = self.store.clone();
        store.update(cx, |store, cx| store.show_alone(thread.to_string(), cx));
        chat.update(cx, |chat, cx| {
            chat.set_session(Some(thread.to_string()), cx)
        });
        self.driver
            .wait_until(cx, "the bound thread's history", |cx| {
                store.read_with(cx, |store, cx| {
                    !store.chat_loading_for(store.sole_thread().as_deref(), cx)
                })
            });
    }

    pub(in crate::chat) fn shutdown(mut self, cx: &mut gpui::VisualTestContext) {
        self.driver.shutdown(cx, &self.host);
    }
}

pub(in crate::chat) fn seed_chat_with_history(
    cx: &mut TestAppContext,
    timeline: Timeline,
    paged: bool,
) -> (
    Entity<WorkspaceStore>,
    Entity<WindowState>,
    String,
    crate::store::tests::LocalHostDriver,
) {
    seed_chat_with_history_on(cx, timeline, paged, None)
}

pub(in crate::chat) fn seed_chat_with_history_on(
    cx: &mut TestAppContext,
    timeline: Timeline,
    paged: bool,
    provider: Option<agent::ProviderKind>,
) -> (
    Entity<WorkspaceStore>,
    Entity<WindowState>,
    String,
    crate::store::tests::LocalHostDriver,
) {
    let (store, window_state, session_id, driver, _) =
        seed_chat_scripted(cx, log_of(&timeline), paged, provider, false);
    (store, window_state, session_id, driver)
}

/// [`seed_chat_with_history_on`] for a thread recorded as `log`.
pub(in crate::chat) fn seed_chat_log_on(
    cx: &mut TestAppContext,
    log: Vec<agent::AgentEvent>,
    provider: Option<agent::ProviderKind>,
) -> (
    Entity<WorkspaceStore>,
    Entity<WindowState>,
    String,
    crate::store::tests::LocalHostDriver,
) {
    let (store, window_state, session_id, driver, _) =
        seed_chat_scripted(cx, log, false, provider, false);
    (store, window_state, session_id, driver)
}

/// The commands a scripted provider receives and the events it sends.
pub(crate) type ScriptedHandles = (
    smol::channel::Receiver<agent::SessionCommand>,
    smol::channel::Sender<agent::AgentEvent>,
);

/// [`seed_chat_with_history_on`], whose provider, when `script`, is a
/// scripted one the test drives: a provider the host starts (for a
/// rewind preview, say) is never the real CLI.
pub(in crate::chat) fn seed_chat_scripted(
    cx: &mut TestAppContext,
    log: Vec<agent::AgentEvent>,
    paged: bool,
    provider: Option<agent::ProviderKind>,
    script: bool,
) -> (
    Entity<WorkspaceStore>,
    Entity<WindowState>,
    String,
    crate::store::tests::LocalHostDriver,
    Option<ScriptedHandles>,
) {
    let scripted = provider
        .filter(|_| script)
        .map(tcode_runtime::app::scripted_provider);
    let launcher = scripted
        .as_ref()
        .map(|scripted| scripted.launcher.clone())
        .unwrap_or_default();
    let (host, session_id) = seeded_chat_host_on(cx, log, paged, provider, launcher);
    let scripted = scripted.map(|scripted| (scripted.commands.clone(), scripted.events.clone()));
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let workspace_store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    driver.wait_until(cx, "workspace baseline", |cx| {
        workspace_store.read_with(cx, |store, _| store.baseline_ready())
    });
    if paged {
        let link = driver.link.clone();
        driver
            .await_with_events(
                cx,
                link.query(tcode_protocol::Query::SessionHistoryPage {
                    session_id: session_id.clone(),
                    before: u64::MAX,
                    limit: 1,
                }),
            )
            .expect("the seeded history is read");
    }
    workspace_store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
    driver.wait_until(cx, "selected conversation", |cx| {
        workspace_store.read_with(cx, |store, cx| {
            store.baseline_ready() && !store.chat_loading_for(store.sole_thread().as_deref(), cx)
        })
    });
    driver.wait_until(cx, "Git status", |cx| {
        workspace_store.read_with(cx, |store, cx| {
            store
                .chat_git_controls_for(store.sole_thread().as_deref(), cx)
                .is_some()
        })
    });
    let window_state = cx.new(|_| WindowState::default());
    let link = driver.link.clone();
    cx.on_quit(move || {
        let events = link.events();
        smol::block_on(smol::future::race(
            link.pump(),
            smol::future::race(
                async { link.shutdown().await.expect("stop chat test host") },
                async {
                    // The UI is gone at test teardown; drain its retired subscriptions.
                    while events.recv().await.is_ok() {}
                    std::future::pending::<()>().await;
                },
            ),
        ));
        host.stopped
            .recv_blocking()
            .expect("chat test host stopped");
    });
    (workspace_store, window_state, session_id, driver, scripted)
}

use gpui::VisualTestContext;
use tcode_core::session::StoredEvent;
use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, Topic};

/// A thread whose host is scripted: its window arrives as a snapshot,
/// then records stream in one at a time, as a host sends them.
pub(in crate::chat) struct ScriptedThread {
    pub(in crate::chat) store: Entity<WorkspaceStore>,
    pub(in crate::chat) incoming: async_channel::Sender<String>,
    pub(in crate::chat) records: Vec<StoredEvent>,
    /// The first record the window holds; the ones before it are history
    /// pages.
    pub(in crate::chat) held_from: usize,
    pub(in crate::chat) status: tcode_protocol::SessionStatus,
    pub(in crate::chat) outgoing: async_channel::Receiver<String>,
    _pump: gpui::Task<()>,
}

pub(in crate::chat) const THREAD: &str = "scripted";

pub(in crate::chat) fn events_topic() -> Topic {
    Topic::SessionEvents {
        session_id: THREAD.into(),
    }
}

pub(in crate::chat) fn user_event(id: &str, text: &str) -> agent::AgentEvent {
    agent::AgentEvent::ItemCompleted(agent::ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: agent::ItemContent::UserMessage {
            text: text.into(),
            attachments: Vec::new(),
        },
    })
}

pub(in crate::chat) fn answer_event(id: &str, text: &str) -> agent::AgentEvent {
    agent::AgentEvent::ItemCompleted(agent::ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: agent::ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        },
    })
}

impl ScriptedThread {
    /// The thread holding `events`, its window starting at record
    /// `held_from`.
    pub(in crate::chat) fn new(
        cx: &mut TestAppContext,
        events: Vec<agent::AgentEvent>,
        held_from: usize,
        running: bool,
    ) -> Self {
        cx.update(crate::test_client::init);
        cx.update(crate::markdown::init);
        let mut status = crate::store::tests::draft_status();
        status.session_id = THREAD.into();
        status.draft = false;
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            std::env::temp_dir(),
            None,
        );
        meta.id = THREAD.into();

        let (to_host, outgoing) = async_channel::unbounded::<String>();
        let (incoming, from_host) = async_channel::unbounded::<String>();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump = cx.background_executor.spawn({
            let link = link.clone();
            async move {
                link.pump().await;
            }
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        store.update(cx, |store, cx| store.show_alone(THREAD.into(), cx));
        let mut thread = Self {
            store,
            incoming,
            records: Vec::new(),
            held_from,
            status,
            outgoing,
            _pump: pump,
        };
        thread.event(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        );
        thread.event(
            Topic::Index,
            ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                summary: Default::default(),
                sessions: vec![meta],
                projects: vec![],
            }),
        );
        thread.send_window(events);
        thread.set_running(running);
        thread
    }

    pub(in crate::chat) fn event(&self, topic: Topic, event: ServerEvent) {
        self.incoming
            .try_send(
                tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }

    pub(in crate::chat) fn turns(&self) -> u64 {
        self.records
            .iter()
            .filter(|record| matches!(record.event, agent::AgentEvent::TurnStarted { .. }))
            .count() as u64
    }

    /// The host's status for the thread: whether its last turn runs.
    pub(in crate::chat) fn set_running(&mut self, running: bool) {
        self.status.activity.turn_running = running;
        self.status.running_turn = running.then(|| tcode_core::session::RunningTurn {
            turn: self.turns() - 1,
            started_at: None,
        });
        self.event(
            Topic::SessionStatus {
                session_id: THREAD.into(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(self.status.clone())),
        );
    }

    /// The host sends its whole window, holding `events` after what it
    /// sent before: what a client folds when it opens the thread, or
    /// when it catches up on the records it missed.
    pub(in crate::chat) fn send_window(&mut self, events: Vec<agent::AgentEvent>) {
        self.records
            .extend(events.into_iter().map(StoredEvent::from));
        let end = self.records.len() as u64;
        let total_turns = self.turns();
        self.event(
            events_topic(),
            ServerEvent::SessionSnapshot {
                from: self.held_from as u64,
                end,
                records: self.records[self.held_from..].to_vec(),
                total: end,
                total_turns,
                truncated: false,
            },
        );
    }

    /// The host sends `events` live, one record at a time, as it records
    /// them.
    pub(in crate::chat) fn stream(&mut self, events: Vec<agent::AgentEvent>) {
        for event in events {
            let record = StoredEvent::from(event);
            self.records.push(record.clone());
            self.event(events_topic(), ServerEvent::SessionEvent(record));
        }
    }

    /// Answer the history page the client asked for with at most `records`
    /// of the records before the window, and say where it ended. `None`
    /// when it asked for none. A client keeps one page in flight. The turn
    /// marks it asked for meanwhile are answered from the whole log, as a
    /// host folds them.
    pub(in crate::chat) fn answer_page(&mut self, records: usize) -> Option<u64> {
        use tcode_protocol::{ClientPayload, Query, QueryResponse};
        let asked: Vec<_> = std::iter::from_fn(|| self.outgoing.try_recv().ok())
            .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
            .filter_map(|message| match message.payload {
                ClientPayload::Query(Query::SessionHistoryPage { before, .. }) => {
                    Some((message.id, before))
                }
                ClientPayload::Query(Query::ThreadTurnMarks { .. }) => {
                    let marks =
                        tcode_core::turn_marks::turn_marks(&Timeline::fold_stored(&self.records));
                    self.incoming
                        .try_send(
                            tcode_protocol::encode_line(&HostMessage::QueryResult {
                                id: message.id,
                                result: Ok(QueryResponse::ThreadTurnMarks(marks)),
                            })
                            .unwrap(),
                        )
                        .unwrap();
                    None
                }
                _ => None,
            })
            .collect();
        assert!(asked.len() <= 1, "one page in flight: {asked:?}");
        let (id, before) = *asked.first()?;
        let from = (before as usize).saturating_sub(records);
        self.held_from = from;
        self.incoming
            .try_send(
                tcode_protocol::encode_line(&HostMessage::QueryResult {
                    id,
                    result: Ok(QueryResponse::SessionHistoryPage {
                        records: self.records[from..before as usize].to_vec(),
                        from: from as u64,
                        end: before,
                        truncated: false,
                    }),
                })
                .unwrap(),
            )
            .unwrap();
        Some(before)
    }

    /// Deliver what the host sent.
    pub(in crate::chat) fn deliver(&self, cx: &mut VisualTestContext) {
        cx.run_until_parked();
    }

    /// A chat view on the thread, at desktop size.
    pub(in crate::chat) fn open<'a>(
        &self,
        cx: &'a mut TestAppContext,
    ) -> (Entity<ChatView>, &'a mut VisualTestContext) {
        self.open_at(gpui::size(gpui::px(1_024.), gpui::px(700.)), cx)
    }

    pub(in crate::chat) fn open_at<'a>(
        &self,
        size: gpui::Size<gpui::Pixels>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<ChatView>, &'a mut VisualTestContext) {
        let store = self.store.clone();
        let window_state = cx.new(|_| WindowState::default());
        let (view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(store, window_state, Some(THREAD.to_owned()), window, cx)
        });
        cx.simulate_resize(size);
        (view, cx)
    }
}
