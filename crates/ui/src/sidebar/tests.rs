use crate::sidebar::row::{RowStatus, StatusKind};

use super::*;
use agent::ProviderKind;
use gpui::{TestAppContext, VisualTestContext, size};
use std::path::PathBuf;
use tcode_core::project::Project;
use tcode_runtime::pipe::{HostServices, spawn_host};
use tcode_services::store::SessionStore;

/// Thread `id` of the host in this process, which these sidebars list.
fn local(id: impl Into<String>) -> ThreadKey {
    ThreadKey {
        host: HostKey::Local,
        session_id: id.into(),
    }
}

impl SessionsSidebar {
    /// The session ids [`SessionsSidebar::navigation_threads`] lists.
    fn navigation_ids(&mut self, cx: &mut Context<Self>) -> Vec<String> {
        self.navigation_threads(cx)
            .into_iter()
            .map(|thread| thread.session_id)
            .collect()
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
}

fn session(id: &str, parent_id: Option<&str>) -> SessionMeta {
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/project"), None);
    meta.id = id.to_string();
    meta.title = id.to_string();
    meta.parent_session_id = parent_id.map(str::to_string);
    meta
}

/// A store over a link the test speaks for the host on: it holds
/// `sessions` in `project` with `summary`'s activity, and `send` says more.
struct ScriptedHost {
    store: Entity<WorkspaceStore>,
    incoming: async_channel::Sender<String>,
    _outgoing: async_channel::Receiver<String>,
    _pump: gpui::Task<()>,
}

impl ScriptedHost {
    fn new(
        cx: &mut TestAppContext,
        sessions: Vec<SessionMeta>,
        project: Project,
        summary: tcode_protocol::IndexSummary,
    ) -> Self {
        use tcode_protocol::{IndexSnapshot, ServerEvent, Topic};
        cx.update(crate::test_client::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let send = |topic, event| Self::say(&incoming, topic, event);
        send(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        );
        send(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                sessions,
                projects: vec![project],
                summary,
            }),
        );
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
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
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        Self {
            store,
            incoming,
            _outgoing: outgoing,
            _pump: pump,
        }
    }

    fn say(
        incoming: &async_channel::Sender<String>,
        topic: tcode_protocol::Topic,
        event: tcode_protocol::ServerEvent,
    ) {
        use tcode_protocol::{EventEnvelope, HostMessage, encode_line};
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }

    fn send(&self, topic: tcode_protocol::Topic, event: tcode_protocol::ServerEvent) {
        Self::say(&self.incoming, topic, event);
    }
}

/// A thread's activity with nothing of it going on.
fn at_rest() -> tcode_protocol::SessionActivity {
    tcode_protocol::SessionActivity {
        working: false,
        turn_running: false,
        background: Default::default(),
        unsettled: 0,
        wakes: Default::default(),
        waiting_for_approval: false,
        waiting_for_input: false,
        asking: Vec::new(),
        settle_blocked: false,
        failed: false,
        unread: false,
        last_activity_at: None,
        fork: tcode_protocol::ForkAvailability::Available,
        agent: None,
        latest_activity: None,
    }
}

/// The phone row of thread `id` as the list last built it.
fn phone_row(sidebar: &SessionsSidebar, id: &str) -> Rc<CompactThreadRow> {
    sidebar
        .compact_model
        .as_ref()
        .unwrap()
        .rows
        .iter()
        .find_map(|row| match row {
            CompactListRow::Thread(row) if row.state.thread.session_id == id => Some(row.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the phone lists {id}"))
}

#[gpui::test]
fn a_phone_row_says_whether_its_thread_works_runs_background_work_or_waits(
    cx: &mut TestAppContext,
) {
    use tcode_protocol::{IndexSummary, ServerEvent, Topic};
    // The thread runs a turn, or only its background work goes on, or
    // nothing of it runs and a watched pull request will wake it.
    #[derive(Clone, Copy, PartialEq)]
    enum Doing {
        Turn,
        Background,
        Watch,
    }
    let summary = |doing: Doing| IndexSummary {
        activity: HashMap::from([(
            "background".into(),
            tcode_protocol::SessionActivity {
                working: doing == Doing::Turn,
                turn_running: doing == Doing::Turn,
                background: tcode_protocol::BackgroundActivity {
                    tasks: (doing == Doing::Background).into(),
                    agents: 0,
                },
                wakes: tcode_protocol::ThreadWakes {
                    pull_requests: if doing == Doing::Watch {
                        vec![tcode_core::pull_request::PullRequestKey::new(
                            "github.com",
                            "sample/project",
                            1048,
                        )]
                    } else {
                        Vec::new()
                    },
                    ..Default::default()
                },
                ..at_rest()
            },
        )]),
        ..Default::default()
    };
    let project = Project::from_root(PathBuf::from("/project"));
    let mut meta = session("background", None);
    meta.project_id = Some(project.id.clone());
    let host = ScriptedHost::new(cx, vec![meta], project, summary(Doing::Background));
    let store = host.store.clone();
    store.update(cx, |store, cx| store.show_alone("background".into(), cx));
    let window_state = cx.new(|_| WindowState::default().with_compact(true));
    let (sidebar, cx) = cx.add_window_view(|_, cx| {
        SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
    });
    cx.simulate_resize(size(px(393.), px(852.)));
    for (doing, expected) in [
        (Doing::Background, StatusKind::Background),
        (Doing::Turn, StatusKind::Working),
        (Doing::Watch, StatusKind::Waiting),
    ] {
        host.send(
            Topic::Index,
            ServerEvent::IndexSummaryReplaced(summary(doing)),
        );
        cx.run_until_parked();
        cx.run_until_parked();
        draw(cx);
        assert!(cx.debug_bounds("compact-row-background").is_some());
        sidebar.read_with(cx, |sidebar, _| {
            let row = phone_row(sidebar, "background");
            let status = RowStatus::of(&row.state, row.working).unwrap();
            assert_eq!(status.kind, expected);
        });
    }
}

/// A dispatched agent that waits on the user is named on its lead's row,
/// after the verb: the verb keeps its whole width however long the agent's
/// title, the model stays named after them, and the row keeps its height
/// beside one that says nothing.
#[gpui::test]
fn a_lead_row_names_its_waiting_agent_after_a_whole_verb(cx: &mut TestAppContext) {
    let long = "Implement CSV export for every report ".repeat(12);
    let project = Project::from_root(PathBuf::from("/project"));
    let sessions = [
        ("idle", None, "idle"),
        ("lead-short", None, "lead-short"),
        ("agent-short", Some("lead-short"), "Explore"),
        ("lead-long", None, "lead-long"),
        ("agent-long", Some("lead-long"), long.as_str()),
    ]
    .map(|(id, parent, title)| {
        let mut meta = session(id, parent);
        meta.title = title.into();
        meta.project_id = Some(project.id.clone());
        meta
    });
    let waiting = tcode_protocol::SessionActivity {
        waiting_for_input: true,
        ..at_rest()
    };
    let summary = tcode_protocol::IndexSummary {
        activity: HashMap::from([
            ("agent-short".into(), waiting.clone()),
            ("agent-long".into(), waiting),
        ]),
        ..Default::default()
    };
    let host = ScriptedHost::new(cx, sessions.to_vec(), project, summary);
    let store = host.store.clone();
    let window_state = cx.new(|_| WindowState::default());
    let (sidebar, cx) = cx.add_window_view(|_, cx| {
        SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
    });
    for (compact, width, row) in [(false, 320., "sidebar-thread"), (true, 393., "compact-row")] {
        window_state.update(cx, |state, cx| state.set_compact(compact, cx));
        cx.simulate_resize(size(px(width), px(800.)));
        crate::test_frames::draw_settled(cx);
        let bounds = |selector: String, cx: &mut VisualTestContext| {
            cx.debug_bounds(selector.clone().leak())
                .unwrap_or_else(|| panic!("{selector} is drawn"))
        };
        assert_eq!(
            bounds(format!("{row}-idle"), cx).size.height,
            bounds(format!("{row}-lead-long"), cx).size.height,
            "a row is as tall with a status as without"
        );
        let (short, long_verb) = (
            bounds("row-status-verb-lead-short".into(), cx),
            bounds("row-status-verb-lead-long".into(), cx),
        );
        assert_eq!(
            short.size.width, long_verb.size.width,
            "a long name takes none of the verb's width"
        );
        for lead in ["lead-short", "lead-long"] {
            let verb = bounds(format!("row-status-verb-{lead}"), cx);
            let model = bounds(format!("row-model-{lead}"), cx);
            let row = bounds(format!("{row}-{lead}"), cx);
            assert!(bounds(format!("row-lamp-{lead}"), cx).right() <= verb.left());
            assert!(
                verb.right() <= model.left()
                    && model.right() <= row.right()
                    && model.size.width > px(0.),
                "{lead}'s row names its model after what it does"
            );
        }
    }
    sidebar.read_with(cx, |sidebar, _| {
        for (lead, agent) in [("lead-short", "Explore"), ("lead-long", long.as_str())] {
            let row = phone_row(sidebar, lead);
            assert_eq!(
                RowStatus::of(&row.state, row.working),
                Some(RowStatus {
                    kind: StatusKind::Answer,
                    name: Some(agent.to_owned().into()),
                }),
                "{lead}'s row names its agent"
            );
        }
    });
    assert!(
        cx.debug_bounds("compact-row-agent-short").is_none(),
        "the agent has no row of its own"
    );
}

/// A host of its own holding `threads` (id and last activity) in one
/// project at a fresh root, and a store over it as `attachment` names it.
struct TestHost {
    store: Entity<WorkspaceStore>,
    driver: crate::store::tests::LocalHostDriver,
    host: tcode_runtime::pipe::SpawnedHost,
    root: PathBuf,
}

impl TestHost {
    fn spawn(
        cx: &mut TestAppContext,
        name: &str,
        attachment: crate::store::WorkspaceAttachment,
        threads: &[(&str, u64)],
    ) -> Self {
        let (host, root) = Self::host(name, threads);
        let driver = crate::store::tests::LocalHostDriver::new(&host);
        Self::attach(cx, host, driver, attachment, root)
    }

    /// A host holding one project with `threads` in it.
    fn host(name: &str, threads: &[(&str, u64)]) -> (tcode_runtime::pipe::SpawnedHost, PathBuf) {
        let root = crate::store::tests::scratch_root(name);
        let session_store = SessionStore::open_at(root.clone()).unwrap();
        let project = Project::from_root(root.clone());
        let sessions: Vec<SessionMeta> = threads
            .iter()
            .map(|(id, at)| {
                let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
                meta.project_id = Some(project.id.clone());
                meta.id = (*id).into();
                meta.title = (*id).into();
                meta.created_at = *at;
                meta.updated_at = *at;
                meta
            })
            .collect();
        session_store.upsert_project(&project).unwrap();
        for meta in &sessions {
            session_store.upsert_meta(meta).unwrap();
        }
        let host =
            spawn_host(session_store, HostServices::default()).expect("spawn sidebar test host");
        (host, root)
    }

    fn attach(
        cx: &mut TestAppContext,
        host: tcode_runtime::pipe::SpawnedHost,
        mut driver: crate::store::tests::LocalHostDriver,
        attachment: crate::store::WorkspaceAttachment,
        root: PathBuf,
    ) -> Self {
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(driver.link.clone(), attachment, None, None, cx)
        });
        driver.wait_until(cx, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        Self {
            store,
            driver,
            host,
            root,
        }
    }

    /// A machine of its own, `host_id`, to the client.
    fn remote(cx: &mut TestAppContext, host_id: &str, threads: &[(&str, u64)]) -> Self {
        let attachment = crate::store::WorkspaceAttachment::Remote {
            host_id: host_id.into(),
            host_name: format!("{host_id} name"),
        };
        Self::spawn(cx, &format!("sidebar-{host_id}"), attachment, threads)
    }

    /// Machine `host_id`, reached as a device of its group while it lets
    /// those devices only view it: attached through the machine's mux as
    /// such a device is.
    fn viewed(cx: &mut TestAppContext, host_id: &str, threads: &[(&str, u64)]) -> Self {
        let (host, root) = Self::host(&format!("sidebar-{host_id}"), threads);
        let mux =
            tcode_traverse::mux::HostMux::new(host.attachments.clone(), host.from_host.clone());
        let connection = mux.attach(
            tcode_protocol::Principal::Viewer {
                device_id: "phone".into(),
                device_name: "Phone".into(),
            },
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let driver = crate::store::tests::LocalHostDriver::from_link(tcode_client::HostLink::new(
            connection.to_host,
            connection.from_host,
        ));
        let attachment = crate::store::WorkspaceAttachment::Remote {
            host_id: host_id.into(),
            host_name: format!("{host_id} name"),
        };
        Self::attach(cx, host, driver, attachment, root)
    }

    /// Session ids in the order the host lists them.
    fn listed(&self, cx: &mut TestAppContext) -> Vec<String> {
        self.store.read_with(cx, |store, _| {
            store
                .flat_sessions()
                .iter()
                .map(|meta| meta.id.clone())
                .collect()
        })
    }

    fn shutdown(mut self, cx: &mut TestAppContext) {
        self.driver
            .link
            .set_connection_state(tcode_client::ConnectionState::Connected { path: None });
        self.driver.shutdown(cx, &self.host);
        let _ = std::fs::remove_dir_all(self.root);
    }
}

/// A desktop sidebar over two machines, `host-a` holding `a` and
/// `host-b` holding `b`, drawn in a window of `height` in `layout`.
fn sidebar_over_two_hosts<'a>(
    cx: &'a mut TestAppContext,
    layout: SidebarLayout,
    a: &[(&str, u64)],
    b: &[(&str, u64)],
    height: f32,
) -> (
    Entity<SessionsSidebar>,
    Entity<WindowState>,
    TestHost,
    TestHost,
    &'a mut VisualTestContext,
) {
    cx.update(crate::test_client::init);
    cx.update(|cx| DevicePreferences::set_sidebar_layout(layout, cx));
    let (a, b) = (
        TestHost::remote(cx, "host-a", a),
        TestHost::remote(cx, "host-b", b),
    );
    let window_state = cx.new(|_| WindowState::default());
    let stores = vec![a.store.clone(), b.store.clone()];
    let sidebar = cx.new(|cx| SessionsSidebar::new(stores, window_state.clone(), cx));
    let (_, cx) =
        cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar.clone(), window, cx));
    cx.simulate_resize(size(px(320.), px(height)));
    crate::test_frames::draw_settled(cx);
    (sidebar, window_state, a, b, cx)
}

#[gpui::test]
fn canceling_inline_rename_on_the_phone_discards_the_unsaved_title(cx: &mut TestAppContext) {
    cx.update(crate::test_client::init);
    cx.update(crate::window_seam::force_mobile_layout);
    let root = crate::store::tests::scratch_root("sidebar-rename-test");
    let session_store = SessionStore::open_at(root.clone()).unwrap();
    let project = Project::from_root(root.clone());
    let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
    meta.project_id = Some(project.id.clone());
    meta.title = "Original title".into();
    meta.id = "rename-thread".into();
    session_store.upsert_project(&project).unwrap();
    session_store.upsert_meta(&meta).unwrap();
    let host = spawn_host(session_store, HostServices::default()).expect("spawn sidebar test host");
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
    driver.wait_until(cx, "sidebar baseline", |cx| {
        store.read_with(cx, |store, _| store.baseline_ready())
    });

    let window_state = cx.new(|_| WindowState::default());
    let sidebar = cx.new(|cx| SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx));
    let (_, cx) =
        cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar.clone(), window, cx));
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(360.), px(800.)));
    window_state.update(cx, |state, cx| {
        state.set_compact(true, cx);
        state.go(crate::window_state::Destination::Threads, cx);
    });
    draw(cx);
    let row = cx.debug_bounds("compact-row-rename-thread").unwrap();
    let trigger = row.center();
    let touch = |cx: &mut VisualTestContext, phase, position| {
        cx.simulate_event(gpui::TouchEvent {
            id: gpui::TouchId(1),
            phase,
            position,
            predicted_position: None,
            force: None,
        });
        draw(cx);
    };
    touch(cx, gpui::TouchPhase::Started, trigger);
    cx.executor().advance_clock(
        gpui::GestureTuning::default().long_press_duration + std::time::Duration::from_millis(1),
    );
    cx.run_until_parked();
    draw(cx);
    touch(cx, gpui::TouchPhase::Ended, trigger);
    crate::test_frames::draw_settled(cx);
    let rename = cx
        .debug_bounds("menu-item-0")
        .expect("thread menu opens")
        .center();
    touch(cx, gpui::TouchPhase::Started, rename);
    touch(cx, gpui::TouchPhase::Ended, rename);
    crate::test_frames::draw_settled(cx);
    cx.simulate_input("Edited");
    assert_eq!(
        sidebar.read_with(cx, |sidebar, cx| sidebar
            .renaming
            .as_ref()
            .expect("Rename starts editing")
            .input
            .read(cx)
            .value()
            .to_string()),
        "Edited",
        "Rename starts with the whole title selected, so typing replaces it"
    );
    let caret = sidebar.read_with(cx, |sidebar, cx| {
        sidebar
            .renaming
            .as_ref()
            .unwrap()
            .input
            .read(cx)
            .text_bounds()
            .unwrap()
            .center()
    });
    touch(cx, gpui::TouchPhase::Started, caret);
    touch(cx, gpui::TouchPhase::Ended, caret);
    assert_eq!(
        window_state.read_with(cx, |state, _| state.destination()),
        crate::window_state::Destination::Threads,
        "tapping the rename editor must not open the thread"
    );
    cx.simulate_click(caret, gpui::Modifiers::default());
    draw(cx);
    assert_eq!(
        window_state.read_with(cx, |state, _| state.destination()),
        crate::window_state::Destination::Threads,
        "clicking the rename editor must not open the thread"
    );
    cx.update(|window, cx| {
        sidebar.update(cx, |sidebar, cx| {
            let input = sidebar
                .renaming
                .as_ref()
                .expect("Rename starts editing")
                .input
                .clone();
            input.update(cx, |input, cx| input.set_value("Unsaved title", window, cx));
        });
    });

    draw(cx);
    sidebar.read_with(cx, |sidebar, cx| {
        let input = &sidebar
            .renaming
            .as_ref()
            .expect("mounted rename editor")
            .input;
        assert_eq!(input.read(cx).value().as_str(), "Unsaved title");
    });
    // This is the supported cancellation path: pointer-down outside the
    // mounted Input triggers its owning row's cancel handler.
    cx.simulate_click(gpui::point(px(340.), px(760.)), gpui::Modifiers::default());
    draw(cx);
    cx.update(|_, cx| {
        assert!(sidebar.read(cx).renaming.is_none());
    });
    driver.round_trip(cx);
    assert_eq!(
        store.read_with(cx, |store, _| store
            .thread_meta("rename-thread")
            .map(|meta| meta.title.clone())),
        Some("Original title".into()),
        "the host keeps the title the rename never saved"
    );

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
}

#[gpui::test]
fn dismissing_a_phone_rows_menu_keeps_the_selection_and_the_next_tap_opens(
    cx: &mut TestAppContext,
) {
    use gpui::{PlatformInput, TouchEvent, TouchId, TouchPhase};
    cx.update(crate::test_client::init);
    let root = crate::store::tests::scratch_root("menu-selection");
    let disk = SessionStore::open_at(root.clone()).unwrap();
    let project = Project::from_root(root.clone());
    let sessions: Vec<_> = (0..12)
        .map(|index| {
            let mut meta = session(&format!("menu-{index}"), None);
            meta.project_id = Some(project.id.clone());
            meta.updated_at = 1000 - index;
            meta.created_at = meta.updated_at;
            meta
        })
        .collect();
    disk.upsert_project(&project).unwrap();
    for meta in &sessions {
        disk.upsert_meta(meta).unwrap();
    }
    let host = spawn_host(disk, HostServices::default()).unwrap();
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
    crate::store::tests::place_shown_threads(&store, cx);
    driver.wait_until(cx, "sidebar baseline", |cx| {
        store.read_with(cx, |store, _| store.baseline_ready())
    });
    let navigation = cx.new(|_| WindowState::default().with_compact(true));
    let (_, cx) = cx
        .add_window_view(|_, cx| SessionsSidebar::new(vec![store.clone()], navigation.clone(), cx));
    cx.simulate_resize(size(px(393.), px(852.)));
    draw(cx);
    // A remote host may not have sent status yet. Selection must repaint
    // from the client notification without waiting for a host event.
    driver.shutdown(cx, &host);
    draw(cx);
    let a = cx.debug_bounds("compact-row-menu-0").unwrap();
    let b = cx.debug_bounds("compact-row-menu-8").unwrap();
    let send = |cx: &mut VisualTestContext, id, phase, position| {
        cx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::Touch(TouchEvent {
                    id: TouchId(id),
                    phase,
                    position,
                    predicted_position: None,
                    force: None,
                }),
                cx,
            );
        })
    };
    send(cx, 1, TouchPhase::Started, a.center());
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(801));
    cx.run_until_parked();
    send(cx, 1, TouchPhase::Ended, a.center());
    draw(cx);
    let menu = cx
        .debug_bounds("tcode-popup-menu")
        .expect("long press opens menu");
    let outside = gpui::point(b.left() + px(12.), b.center().y);
    assert!(!menu.contains(&outside));
    let selected = store.read_with(cx, |store, _| store.open_threads());
    let destination = navigation.read_with(cx, |state, _| state.destination());
    send(cx, 2, TouchPhase::Started, outside);
    send(cx, 2, TouchPhase::Ended, outside);
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("tcode-popup-menu").is_none());
    assert_eq!(
        store.read_with(cx, |store, _| store.open_threads()),
        selected
    );
    assert_eq!(
        navigation.read_with(cx, |state, _| state.destination()),
        destination
    );
    send(cx, 3, TouchPhase::Started, b.center());
    send(cx, 3, TouchPhase::Ended, b.center());
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        store.read_with(cx, |store, _| store.open_threads()),
        ["menu-8"]
    );
    let _ = std::fs::remove_dir_all(root);
}

fn compact_list_has_project_headers(
    cx: &mut TestAppContext,
    projects: usize,
    quick_chat: bool,
) -> bool {
    use tcode_client::host::ClientHost as _;
    cx.update(crate::test_client::init);
    let root = crate::store::tests::scratch_root(&format!("sidebar-{projects}-project"));
    let device = || tcode_traverse::NativeClientHost::new(root.join("device"), "phone");
    let client: std::rc::Rc<dyn tcode_client::host::ClientHost> = std::rc::Rc::new(device());
    cx.update(|cx| DevicePreferences::install(client, cx));
    // The host's quick threads work in its documents folder; a project
    // there is the quick-thread project.
    let user_directories = tcode_services::user_files::UserDirectories {
        home: None,
        documents: Some(root.join("documents")),
    };
    let scratch = user_directories.scratch_root().unwrap();
    let seeded: Vec<Project> = (0..projects)
        .map(|index| {
            Project::from_root(if quick_chat && index == 0 {
                scratch.clone()
            } else {
                root.join(format!("project-{index}"))
            })
        })
        .collect();
    let disk = SessionStore::open_at(root.clone()).unwrap();
    for (index, project) in seeded.iter().enumerate() {
        let mut meta = session(&format!("thread-{index}"), None);
        meta.project_id = Some(project.id.clone());
        meta.updated_at = 100 + index as u64;
        meta.created_at = meta.updated_at;
        disk.upsert_project(project).unwrap();
        disk.upsert_meta(&meta).unwrap();
    }
    let host = spawn_host(
        disk,
        HostServices {
            user_directories,
            ..HostServices::default()
        },
    )
    .expect("spawn sidebar test host");

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
    driver.wait_until(cx, "sidebar baseline", |cx| {
        store.read_with(cx, |store, _| store.baseline_ready())
    });
    let window_state = cx.new(|_| WindowState::default().with_compact(true));
    let (sidebar, cx) =
        cx.add_window_view(|_, cx| SessionsSidebar::new(vec![store.clone()], window_state, cx));
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(393.), px(852.)));
    sidebar.update(cx, |_, cx| {
        cx.notify();
    });
    draw(cx);
    assert!(
        cx.debug_bounds("compact-thread-list").is_some(),
        "the seeded threads are listed"
    );
    assert_eq!(
        cx.update(|_, cx| DevicePreferences::sidebar_layout(cx)),
        SidebarLayout::Flat
    );
    assert!(cx.debug_bounds("compact-group-header").is_none());
    for index in 0..projects {
        assert!(
            cx.debug_bounds(if index == 0 {
                if quick_chat {
                    "compact-project-Quick thread"
                } else {
                    "compact-project-project-0"
                }
            } else {
                "compact-project-project-1"
            })
            .is_some(),
            "Recent names each project in the subtitle"
        );
    }
    if projects == 2 {
        assert!(
            cx.debug_bounds("compact-row-thread-1").unwrap().top()
                < cx.debug_bounds("compact-row-thread-0").unwrap().top(),
            "the later thread leads across projects"
        );
    }
    let toggle = cx.debug_bounds("compact-layout-toggle").unwrap();
    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    draw(cx);
    assert_eq!(
        device().load_preferences().unwrap().sidebar_layout,
        Some(SidebarLayout::Grouped),
        "the actual header action persists this device's choice"
    );
    // The list the new layout brings folds its rows in; the reader
    // presses a header once it is there.
    crate::test_frames::draw_settled(cx);
    let headers = cx.debug_bounds("compact-group-header").is_some();
    if quick_chat {
        assert!(store.read_with(cx, |store, _| {
            store.projects().iter().all(|p| !p.quick_chat)
        }));
        assert!(store.read_with(cx, |store, _| {
            store
                .grouped_sessions()
                .iter()
                .any(|g| g.project.quick_chat)
        }));
        assert!(cx.debug_bounds("compact-group-Quick threads").is_some());
    }
    if projects > 1 {
        for collapsed in [true, false] {
            let header = cx.debug_bounds("compact-group-header").unwrap();
            cx.simulate_click(header.center(), gpui::Modifiers::default());
            driver.wait_until(cx, "the project toggle to reach the replica", |cx| {
                store.read_with(cx, |store, _| {
                    seeded
                        .iter()
                        .any(|project| store.is_project_collapsed(&project.id))
                }) == collapsed
            });
            crate::test_frames::draw_settled(cx);
            let visible = ["compact-row-thread-0", "compact-row-thread-1"]
                .into_iter()
                .filter(|selector| cx.debug_bounds(selector).is_some())
                .count();
            assert_eq!(
                visible,
                if collapsed { 1 } else { 2 },
                "the folder action hides and restores its threads"
            );
        }
    }

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
    headers
}

#[gpui::test]
fn compact_recent_orders_projects_and_switches_to_persisted_grouped_view(cx: &mut TestAppContext) {
    assert!(
        !compact_list_has_project_headers(cx, 1, false),
        "a single project needs no header to separate it from anything"
    );
    assert!(
        compact_list_has_project_headers(cx, 2, false),
        "two projects need their headers back"
    );
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    assert!(compact_list_has_project_headers(cx, 1, true));
    assert!(compact_list_has_project_headers(cx, 2, true));
}

/// The sidebar's share item puts the project in a space on the machine the
/// window shows, and takes it out again, and the project is marked shared
/// while it is in one. A space set to hide its projects from the threads
/// view leaves their threads to the project view. The machine answers
/// over a scripted transport.
#[gpui::test]
fn the_share_item_toggles_the_project_in_a_space_on_its_machine(cx: &mut TestAppContext) {
    use tcode_protocol::{
        ClientPayload, CommandResponse, EventEnvelope, HostMessage, HostingAction, HostingState,
        IndexSnapshot, Query, QueryResponse, ServerEvent, SpaceAction, SpaceInfo, Topic,
        decode_client_line, encode_line,
    };
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    cx.update(crate::test_client::init);
    cx.update(|cx| DevicePreferences::set_sidebar_layout(SidebarLayout::Grouped, cx));
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let event = |topic, event| {
        encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic,
            event,
        }))
        .unwrap()
    };
    let mut project = Project::from_root(PathBuf::from("/a"));
    project.id = "a".into();
    let mut thread = session("thread", None);
    thread.project_id = Some(project.id.clone());
    for line in [
        event(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        ),
        event(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions: vec![thread],
                projects: vec![project],
            }),
        ),
    ] {
        incoming.try_send(line).unwrap();
    }
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let pump_link = link.clone();
    let _pump = cx.background_executor.spawn(async move {
        pump_link.pump().await;
    });
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            link,
            crate::store::WorkspaceAttachment::Remote {
                host_id: "studio".into(),
                host_name: "Studio".into(),
            },
            None,
            None,
            cx,
        )
    });
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    let window_state = cx.new(|_| WindowState::default());
    let (sidebar, cx) =
        cx.add_window_view(|_, cx| SessionsSidebar::new(vec![store.clone()], window_state, cx));
    cx.simulate_resize(size(px(320.), px(900.)));

    // The machine's spaces: one, holding the projects it was last given.
    let mut shared: Vec<String> = Vec::new();
    let mut hidden = false;
    let mut asked: Vec<Vec<String>> = Vec::new();
    let mut serve = |cx: &mut VisualTestContext| {
        for _ in 0..4 {
            cx.run_until_parked();
            cx.run_until_parked();
            while let Ok(line) = outgoing.try_recv() {
                let message = decode_client_line(&line).unwrap();
                let state = |shared: &Vec<String>, hidden: bool| HostingState {
                    created_space_id: None,
                    spaces: vec![SpaceInfo {
                        id: "design".into(),
                        name: "Design".into(),
                        created_unix: 1,
                        project_ids: shared.clone(),
                        link: None,
                        link_enabled: true,
                        link_dead: false,
                        members: vec![],
                        policy: tcode_protocol::SpacePolicy::unrestricted(),
                        hidden_from_threads: hidden,
                    }],
                    enabled: true,
                    expires_in_secs: 0,
                    host_id: "studio".into(),
                    host_name: "Studio".into(),
                    invite: None,
                    devices: vec![],
                    traverse: vec![],
                    group: None,
                };
                let reply = match message.payload {
                    ClientPayload::Subscribe(subscription)
                        if subscription.topic == Topic::Hosting =>
                    {
                        HostMessage::Event(EventEnvelope {
                            request_id: Some(message.id),
                            topic: Topic::Hosting,
                            event: ServerEvent::HostingReplaced(Box::new(state(&shared, hidden))),
                        })
                    }
                    ClientPayload::Query(Query::Hosting { action }) => {
                        match action {
                            HostingAction::Spaces(SpaceAction::SetProjects {
                                project_ids, ..
                            }) => {
                                asked.push(project_ids.clone());
                                shared = project_ids;
                            }
                            HostingAction::Spaces(SpaceAction::SetHiddenFromThreads {
                                hidden: next,
                                ..
                            }) => hidden = next,
                            _ => {}
                        }
                        HostMessage::QueryResult {
                            id: message.id,
                            result: Ok(QueryResponse::Hosting(state(&shared, hidden))),
                        }
                    }
                    ClientPayload::Subscribe(_) | ClientPayload::Command(_) => HostMessage::Ack {
                        id: message.id,
                        result: Ok(CommandResponse::Unit),
                    },
                    _ => continue,
                };
                incoming.try_send(encode_line(&reply).unwrap()).unwrap();
            }
        }
        draw(cx);
    };
    serve(cx);
    assert!(cx.debug_bounds("project-header-a").is_some());
    assert!(cx.debug_bounds("project-shared-a").is_none());

    let share = spaces::ToggleShare {
        space_id: "design".into(),
        project_id: "a".into(),
    };
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_share(&share, window, cx)
    });
    serve(cx);
    assert!(
        cx.debug_bounds("project-shared-a").is_some(),
        "a shared project is marked"
    );
    assert!(cx.debug_bounds("sidebar-thread-thread").is_some());
    cx.update(|_, cx| DevicePreferences::set_sidebar_layout(SidebarLayout::Flat, cx));
    serve(cx);
    crate::test_frames::draw_settled(cx);
    assert!(
        cx.debug_bounds("sidebar-thread-thread").is_some(),
        "sharing alone keeps a project in the threads view"
    );

    let spaces = cx.update(|_, cx| spaces::for_store(&store, cx)).unwrap();
    spaces
        .update(cx, |spaces, cx| {
            spaces.act(
                vec![SpaceAction::SetHiddenFromThreads {
                    id: "design".into(),
                    hidden: true,
                }],
                cx,
            )
        })
        .detach();
    serve(cx);
    crate::test_frames::draw_settled(cx);
    assert!(
        cx.debug_bounds("sidebar-thread-thread").is_none(),
        "the threads view leaves a hiding space's projects out"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_share(&share, window, cx)
    });
    serve(cx);
    assert!(
        cx.debug_bounds("sidebar-thread-thread").is_some(),
        "a project out of the hiding space returns to the threads view"
    );
    cx.update(|_, cx| DevicePreferences::set_sidebar_layout(SidebarLayout::Grouped, cx));
    serve(cx);
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("project-shared-a").is_none());
    drop(serve);
    assert_eq!(asked, [vec!["a".to_string()], vec![]]);
}

#[gpui::test]
fn the_grouped_list_keeps_the_readers_place_as_threads_change(cx: &mut TestAppContext) {
    use tcode_protocol::{
        EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
    };
    cx.update(crate::test_client::init);
    let (to_host, _outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let send = |topic, event| {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap()
    };
    let mut project = Project::from_root(PathBuf::from("/project"));
    project.id = "project".into();
    let mut sessions = (0..60)
        .map(|index| {
            let mut meta = session(&format!("virtual-{index}"), None);
            meta.project_id = Some(project.id.clone());
            meta.updated_at = 1_000 - index;
            meta.created_at = meta.updated_at;
            meta
        })
        .collect::<Vec<_>>();
    let mut other = Project::from_root(PathBuf::from("/other"));
    other.id = "other".into();
    let mut other_thread = session("other-0", None);
    other_thread.project_id = Some(other.id.clone());
    other_thread.updated_at = 1;
    sessions.push(other_thread.clone());
    let snapshot = |sessions: &Vec<SessionMeta>| {
        ServerEvent::IndexSnapshot(IndexSnapshot {
            summary: Default::default(),
            sessions: sessions.clone(),
            projects: vec![project.clone(), other.clone()],
        })
    };
    cx.update(|cx| DevicePreferences::set_sidebar_layout(SidebarLayout::Grouped, cx));
    send(
        Topic::Settings,
        ServerEvent::SettingsSnapshot(Default::default()),
    );
    send(Topic::Index, snapshot(&sessions));
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let pump_link = link.clone();
    let _pump = cx.background_executor.spawn(async move {
        pump_link.pump().await;
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
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    store.update(cx, |store, cx| store.show_alone("virtual-0".into(), cx));
    let window_state = cx.new(|_| WindowState::default());
    let (sidebar, cx) =
        cx.add_window_view(|_, cx| SessionsSidebar::new(vec![store.clone()], window_state, cx));
    cx.run_until_parked();
    cx.run_until_parked();
    cx.simulate_resize(size(px(300.), px(800.)));
    sidebar.update(cx, |_, cx| {
        cx.notify();
    });
    draw(cx);
    let more = cx.debug_bounds("show-more-project").unwrap();
    cx.simulate_click(more.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);

    let far_row = |sidebar: &SessionsSidebar| {
        sidebar
            .grouped_row_keys
            .iter()
            .position(|(_, id)| id == "virtual-30")
            .unwrap()
    };
    sidebar.update(cx, |sidebar, cx| {
        sidebar.grouped_list_state.scroll_to(gpui::ListOffset {
            item_ix: far_row(sidebar),
            offset_in_item: px(0.),
        });
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("sidebar-thread-virtual-30").is_some());

    let mut newer = session("virtual-new", None);
    newer.project_id = Some(project.id.clone());
    newer.updated_at = 2_000;
    newer.created_at = 2_000;
    sessions.insert(0, newer);
    let apply = |sessions: &Vec<SessionMeta>, cx: &mut VisualTestContext| {
        send(Topic::Index, snapshot(sessions));
        cx.run_until_parked();
        cx.run_until_parked();
        draw(cx);
    };
    apply(&sessions, cx);
    sidebar.read_with(cx, |sidebar, _| {
        assert_eq!(
            sidebar.grouped_list_state.logical_scroll_top().item_ix,
            far_row(sidebar),
            "a new thread above the viewport keeps the visible thread anchored"
        );
    });
    assert!(cx.debug_bounds("sidebar-thread-virtual-30").is_some());

    let top = sidebar.read_with(cx, |sidebar, _| far_row(sidebar));
    sessions
        .iter_mut()
        .find(|meta| meta.id == "virtual-30")
        .unwrap()
        .archived_at = Some(1);
    apply(&sessions, cx);
    sidebar.read_with(cx, |sidebar, _| {
        assert_eq!(
            sidebar.grouped_list_state.logical_scroll_top().item_ix,
            top,
            "archiving the top thread leaves the list where it was"
        );
    });
    assert!(cx.debug_bounds("sidebar-thread-virtual-31").is_some());

    sidebar.update(cx, |sidebar, cx| {
        sidebar
            .grouped_list_state
            .scroll_to(gpui::ListOffset::default());
        cx.notify();
    });
    draw(cx);
    sessions
        .iter_mut()
        .find(|meta| meta.id == "other-0")
        .unwrap()
        .updated_at = 5_000;
    apply(&sessions, cx);
    sidebar.read_with(cx, |sidebar, _| {
        assert_eq!(sidebar.grouped_row_keys[0].1, "other");
        assert_eq!(
            sidebar.grouped_list_state.logical_scroll_top().item_ix,
            0,
            "a list at its top shows the project that moved up"
        );
    });
    assert!(cx.debug_bounds("sidebar-thread-other-0").is_some());
}

#[gpui::test]
fn agents_are_not_listed(cx: &mut TestAppContext) {
    use tcode_protocol::{
        EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
    };
    cx.update(crate::test_client::init);
    let (to_host, _outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let project = Project::from_root(PathBuf::from("/project"));
    let sessions = [
        ("parent", None),
        ("child", Some("parent")),
        ("mirror", Some("parent")),
        ("other", None),
    ]
    .into_iter()
    .map(|(id, parent)| {
        let mut meta = session(id, parent);
        meta.project_id = Some(project.id.clone());
        meta.native_subagent = (id == "mirror").then(|| "spawn-1".into());
        meta
    })
    .collect();
    let send = |topic, event| {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap()
    };
    send(
        Topic::Settings,
        ServerEvent::SettingsSnapshot(Default::default()),
    );
    send(
        Topic::Index,
        ServerEvent::IndexSnapshot(IndexSnapshot {
            summary: Default::default(),
            sessions,
            projects: vec![project],
        }),
    );
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let pump_link = link.clone();
    let _pump = cx.background_executor.spawn(async move {
        pump_link.pump().await;
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
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    // A selection keeps the store from opening a draft against a host
    // this fake transport never answers.
    store.update(cx, |store, cx| store.show_alone("other".into(), cx));
    let window_state = cx.new(|_| WindowState::default().with_compact(true));
    let (_, cx) = cx.add_window_view(|_, cx| {
        SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
    });
    cx.simulate_resize(size(px(393.), px(852.)));
    cx.run_until_parked();
    cx.run_until_parked();
    draw(cx);
    assert!(cx.debug_bounds("compact-row-parent").is_some());
    assert!(cx.debug_bounds("compact-row-child").is_none());
    assert!(cx.debug_bounds("compact-row-mirror").is_none());
}

#[gpui::test]
fn settled_shelf_pages_without_expanding_for_selection_and_navigation_matches_rows(
    cx: &mut TestAppContext,
) {
    use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, Topic, encode_line};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    cx.update(crate::test_client::init);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let send = |topic, event| {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    };
    let mut project = Project::from_root(PathBuf::from("/sample"));
    project.id = "sample".into();
    let sessions = (0..8)
        .map(|index| {
            let mut meta = session(&format!("active-{index}"), None);
            meta.project_id = Some(project.id.clone());
            meta.created_at = 100 - index;
            meta.updated_at = meta.created_at;
            meta
        })
        .chain((0..40).map(|index| {
            let mut meta = session(&format!("settled-{index}"), None);
            meta.project_id = Some(project.id.clone());
            meta.settled_at = Some(100 - index);
            meta
        }))
        .collect();
    send(
        Topic::Settings,
        ServerEvent::SettingsSnapshot(Default::default()),
    );
    let (index, outside) = crate::store::tests::host_index(
        sessions,
        vec![project, Project::from_root(PathBuf::from("/other-sample"))],
        Default::default(),
    );
    send(Topic::Index, ServerEvent::IndexSnapshot(index));
    let _outside =
        crate::store::tests::answer_outside_index(cx, outgoing, incoming.clone(), outside.clone());
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let pump_link = link.clone();
    let _pump = cx.background_executor.spawn(async move {
        pump_link.pump().await;
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
    crate::shell::tests::shell_behind(&store, cx);
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    // The shell behind shows the thread from a store of its own; this one
    // opens it as the pane showing it would, which reads it from the host.
    store.update(cx, |store, cx| {
        store.show_thread("settled-39".into(), cx);
        store.open_thread("settled-39", cx);
    });
    let window_state = cx.new(|_| WindowState::default());
    let (sidebar, cx) = cx.add_window_view(|_, cx| {
        SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
    });
    cx.simulate_resize(size(px(393.), px(6000.)));
    for compact in [false, true] {
        window_state.update(cx, |state, cx| {
            state.compact = compact;
            cx.notify();
        });
        cx.run_until_parked();
        cx.run_until_parked();
        sidebar.update(cx, |sidebar, cx| {
            sidebar.expanded_settled.clear();
            sidebar.settled_limits.clear();
            sidebar.compact_model_dirty = true;
            cx.notify();
        });
        draw(cx);
        let selector = |id: &str| -> &'static str {
            if compact {
                format!("compact-row-{id}")
            } else {
                format!("sidebar-thread-{id}")
            }
            .leak()
        };
        let expected = |limit: usize| {
            (0..8)
                .map(|index| format!("active-{index}"))
                .chain((0..limit).map(|index| format!("settled-{index}")))
                .chain(std::iter::once("settled-39".into()))
                .collect::<Vec<_>>()
        };
        let verify = |cx: &mut VisualTestContext, limit| {
            let ids = sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx));
            assert_eq!(ids, expected(limit), "compact={compact}");
            let mut previous = None;
            for id in ids {
                let bounds = cx
                    .debug_bounds(selector(&id))
                    .unwrap_or_else(|| panic!("missing visible row {id}"));
                if let Some(previous) = previous {
                    assert!(
                        bounds.top() > previous,
                        "row {id}: top {:?} <= previous {:?}; compact={compact} limit={limit}",
                        bounds.top(),
                        previous
                    );
                }
                previous = Some(bounds.top());
            }
        };
        // A shelf's rows are the ones its host says it holds: the click
        // asks, and the answer is drawn.
        let click = |cx: &mut VisualTestContext, selector: &'static str| {
            let bounds = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            draw(cx);
            cx.run_until_parked();
            draw(cx);
        };
        verify(cx, 0);
        assert!(cx.debug_bounds(selector("settled-0")).is_none());
        click(cx, "settled-recent");
        verify(cx, 10);
        assert!(cx.debug_bounds(selector("settled-10")).is_none());
        click(cx, "settled-more-recent");
        verify(cx, 35);
        assert!(cx.debug_bounds(selector("settled-35")).is_none());
        click(cx, "settled-recent");
        verify(cx, 0);
        click(cx, "settled-recent");
        verify(cx, 10);
        if !compact {
            // Only the desktop's flat list has a project filter.
            sidebar.update(cx, |sidebar, cx| {
                sidebar.project_filter = Some(ProjectKey {
                    host: HostKey::Local,
                    project_id: "missing-project".into(),
                });
                cx.notify();
            });
            draw(cx);
            assert!(
                sidebar
                    .update(cx, |sidebar, cx| sidebar.navigation_ids(cx))
                    .is_empty()
            );
            sidebar.update(cx, |sidebar, cx| {
                sidebar.project_filter = None;
                cx.notify();
            });
            draw(cx);
            verify(cx, 0);
            click(cx, "settled-recent");
        }
        verify(cx, 10);
    }
}

/// Opening or closing the Settled shelf, selecting a row and activity
/// lifting the row at the top edge leave a scrolled threads list exactly
/// where it was, including when the selection takes a viewed settled row
/// out from under the closed shelf.
#[gpui::test]
fn the_threads_list_keeps_its_offset_across_the_shelf_and_selection(cx: &mut TestAppContext) {
    use tcode_protocol::{
        EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
    };
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    cx.update(crate::test_client::init);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let send = |topic, event| {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    };
    let mut project = Project::from_root(PathBuf::from("/sample"));
    project.id = "sample".into();
    let sessions = (0..40)
        .map(|index| {
            let mut meta = session(&format!("active-{index}"), None);
            meta.project_id = Some(project.id.clone());
            meta.updated_at = 1_000 - index;
            meta
        })
        .chain((0..30).map(|index| {
            let mut meta = session(&format!("settled-{index}"), None);
            meta.project_id = Some(project.id.clone());
            meta.settled_at = Some(500 - index);
            meta
        }))
        .collect();
    send(
        Topic::Settings,
        ServerEvent::SettingsSnapshot(Default::default()),
    );
    let (index, outside) =
        crate::store::tests::host_index(sessions, vec![project], Default::default());
    send(Topic::Index, ServerEvent::IndexSnapshot(index));
    let _outside =
        crate::store::tests::answer_outside_index(cx, outgoing, incoming.clone(), outside.clone());
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let pump_link = link.clone();
    let _pump = cx.background_executor.spawn(async move {
        pump_link.pump().await;
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
    crate::store::tests::place_shown_threads(&store, cx);
    crate::shell::tests::shell_behind(&store, cx);
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    // The open thread is settled, so the closed shelf shows its row.
    store.update(cx, |store, cx| store.show_thread("settled-20".into(), cx));
    let window_state = cx.new(|_| WindowState::default());
    let (sidebar, cx) = cx.add_window_view(|_, cx| {
        SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
    });
    cx.run_until_parked();
    cx.run_until_parked();
    cx.simulate_resize(size(px(393.), px(852.)));
    draw(cx);
    let list = sidebar.read_with(cx, |sidebar, _| sidebar.flat_list_state.clone());
    // Near Active's end, the shelf's heading below it on screen.
    list.scroll_to(gpui::ListOffset {
        item_ix: 35,
        offset_in_item: px(20.),
    });
    sidebar.update(cx, |_, cx| cx.notify());
    draw(cx);
    let offset = list.logical_scroll_top();
    let first = |cx: &mut VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar.flat_row_keys[sidebar.flat_list_state.logical_scroll_top().item_ix].clone()
        })
    };
    let top_row = first(cx);
    let unchanged = |cx: &mut VisualTestContext, after: &str| {
        assert_eq!(first(cx), top_row, "the top row after {after}");
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            offset.offset_in_item,
            "the offset after {after}"
        );
    };
    let click = |cx: &mut VisualTestContext, selector: &'static str| {
        let bounds = cx.debug_bounds(selector).unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        draw(cx);
    };

    click(cx, "settled-recent");
    assert!(cx.debug_bounds("sidebar-thread-settled-0").is_some());
    unchanged(cx, "opening the shelf");
    click(cx, "settled-recent");
    assert!(cx.debug_bounds("sidebar-thread-settled-0").is_none());
    unchanged(cx, "closing the shelf");

    // Beyond the shelf's first page, the open thread shows after it.
    click(cx, "settled-recent");
    let listed =
        |cx: &mut VisualTestContext| sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx));
    assert!(listed(cx).contains(&"settled-20".to_string()));
    click(cx, "sidebar-thread-active-35");
    assert_eq!(
        store.read_with(cx, |store, _| store.open_threads()),
        ["active-35"]
    );
    assert!(
        !listed(cx).contains(&"settled-20".to_string()),
        "the settled row left with the selection"
    );
    unchanged(cx, "selecting a row");

    // A message lifts the thread at the top edge to the head of Active;
    // the list stays where it was instead of following it there.
    let (kind, lifted) = top_row.clone();
    assert_eq!(kind, "thread");
    let mut sessions = store.read_with(cx, |store, _| store.sidebar_sessions().to_vec());
    sessions
        .iter_mut()
        .find(|meta| meta.id == lifted)
        .unwrap()
        .updated_at = 2_000;
    send(
        Topic::Index,
        ServerEvent::IndexSnapshot(IndexSnapshot {
            sessions,
            projects: store.read_with(cx, |store, _| store.projects()),
            summary: Default::default(),
        }),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    draw(cx);
    assert_eq!(
        sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx))[0],
        lifted
    );
    let top = list.logical_scroll_top();
    assert_eq!(
        (top.item_ix, top.offset_in_item),
        (offset.item_ix, offset.offset_in_item),
        "the lifted row is not followed"
    );
}

/// The project view lists a project's pinned and active roots as one list
/// by last update and ends it in a closed "Settled N" shelf, newest
/// settled first, that opens per project and opens for a selected settled
/// thread. Projects order by the activity of every thread in them, agents
/// included. A project shows six active rows until "Show more", whatever
/// its shelf holds.
#[gpui::test]
fn the_project_view_keeps_a_settled_shelf_per_project(cx: &mut TestAppContext) {
    use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, Topic, encode_line};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    cx.update(crate::test_client::init);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let send = |topic, event| {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    };
    let projects = ["lead", "mixed", "many"].map(|id| {
        let mut project = Project::from_root(PathBuf::from(format!("/{id}")));
        project.id = id.into();
        project.quick_chat = id == "lead";
        project
    });
    let thread = |id: &str, project: &str, updated: u64| {
        let mut meta = session(id, None);
        meta.project_id = Some(project.into());
        meta.created_at = 1;
        meta.updated_at = updated;
        meta
    };
    let settled = |id: &str, project: &str, updated: u64, settled_at: u64| {
        let mut meta = thread(id, project, updated);
        meta.settled_at = Some(settled_at);
        meta
    };
    let mut pinned = thread("lead-pinned", "lead", 50);
    pinned.pinned_at = Some(50);
    pinned.pin_order = Some("n".into());
    let mut agent = thread("lead-agent", "lead", 900);
    agent.parent_session_id = Some("lead-settled-0".into());
    let mut sessions = vec![
        settled("lead-settled-0", "lead", 60, 60),
        pinned,
        settled("lead-settled-1", "lead", 30, 70),
        settled("lead-settled-2", "lead", 20, 20),
        agent,
        thread("mixed-a", "mixed", 300),
        settled("mixed-old", "mixed", 200, 200),
        thread("mixed-b", "mixed", 100),
    ];
    sessions.extend((0..8).map(|index| thread(&format!("many-{index}"), "many", 10 + index)));
    // Newer than every active row in their project, so a limit that
    // counted them would take active rows' places.
    sessions.push(settled("many-settled-0", "many", 41, 41));
    sessions.push(settled("many-settled-1", "many", 40, 40));
    send(
        Topic::Settings,
        ServerEvent::SettingsSnapshot(Default::default()),
    );
    let (index, outside) =
        crate::store::tests::host_index(sessions, projects.to_vec(), Default::default());
    send(Topic::Index, ServerEvent::IndexSnapshot(index));
    let _outside =
        crate::store::tests::answer_outside_index(cx, outgoing, incoming.clone(), outside.clone());
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let pump_link = link.clone();
    let _pump = cx.background_executor.spawn(async move {
        pump_link.pump().await;
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
    crate::shell::tests::shell_behind(&store, cx);
    // Chosen on this device once its shell has read its preferences.
    cx.update(|cx| DevicePreferences::set_sidebar_layout(SidebarLayout::Grouped, cx));
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    store.update(cx, |store, cx| store.show_thread("mixed-a".into(), cx));
    let window_state = cx.new(|_| WindowState::default());
    let (sidebar, cx) = cx.add_window_view(|_, cx| {
        SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
    });
    cx.run_until_parked();
    cx.run_until_parked();
    cx.simulate_resize(size(px(393.), px(2000.)));
    let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
    let lead = |open: bool| {
        let mut rows = ids(&["lead-pinned"]);
        if open {
            rows.extend(ids(&["lead-settled-1", "lead-settled-0", "lead-settled-2"]));
        }
        rows
    };
    let mixed = ids(&["mixed-a", "mixed-b"]);
    let many = |shown: u64, open: bool| {
        let mut rows = (0..shown)
            .map(|index| format!("many-{}", 7 - index))
            .collect::<Vec<_>>();
        if open {
            rows.extend(ids(&["many-settled-0", "many-settled-1"]));
        }
        rows
    };
    let expected = |shown: u64, lead_open: bool, many_open: bool| {
        lead(lead_open)
            .into_iter()
            .chain(mixed.clone())
            .chain(many(shown, many_open))
            .collect::<Vec<_>>()
    };
    // A shelf's rows are the ones its host says it holds: the click asks,
    // and the answer is drawn.
    let click = |cx: &mut VisualTestContext, selector: &'static str| {
        let bounds = cx.debug_bounds(selector).unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        crate::test_frames::draw_settled(cx);
        cx.run_until_parked();
        crate::test_frames::draw_settled(cx);
    };
    for compact in [false, true] {
        store.update(cx, |store, cx| store.show_thread("mixed-a".into(), cx));
        window_state.update(cx, |state, cx| {
            state.compact = compact;
            cx.notify();
        });
        sidebar.update(cx, |sidebar, cx| {
            sidebar.expanded_settled.clear();
            sidebar.compact_model_dirty = true;
            cx.notify();
        });
        crate::test_frames::draw_settled(cx);
        let shelves = sidebar.update(cx, |sidebar, cx| {
            if compact {
                sidebar
                    .compact_model(None, cx)
                    .rows
                    .iter()
                    .filter_map(|row| match row {
                        CompactListRow::Settled { key, count, .. } => Some((key.clone(), *count)),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            } else {
                let lists = sidebar.thread_lists(cx);
                sidebar
                    .grouped_rows(&lists, cx)
                    .into_iter()
                    .filter_map(|(row, _)| match row {
                        GroupedListRow::Settled { host, group, count } => {
                            Some((lists[host].groups[group].project.id.clone(), count))
                        }
                        _ => None,
                    })
                    .collect()
            }
        });
        assert_eq!(
            shelves,
            [("lead".into(), 3), ("mixed".into(), 1), ("many".into(), 2)],
            "compact={compact}"
        );
        let verify = |cx: &mut VisualTestContext, ids: Vec<String>| {
            assert_eq!(
                sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx)),
                ids,
                "compact={compact}"
            );
            let mut previous = None;
            for id in ids {
                let selector = if compact {
                    format!("compact-row-{id}")
                } else {
                    format!("sidebar-thread-{id}")
                };
                let top = cx.debug_bounds(selector.leak()).unwrap().top();
                assert!(previous.is_none_or(|previous| top > previous), "{id}");
                previous = Some(top);
            }
        };
        assert!(
            cx.debug_bounds(if compact {
                "compact-group-Quick threads"
            } else {
                "quick-chats-header"
            })
            .is_some()
        );
        assert!(cx.debug_bounds("project-header-lead").is_none());
        let shelf = cx.debug_bounds("settled-lead").unwrap().top();
        let pinned = if compact {
            "compact-row-lead-pinned"
        } else {
            "sidebar-thread-lead-pinned"
        };
        assert!(cx.debug_bounds(pinned).unwrap().top() < shelf);
        assert!(cx.debug_bounds("settled-mixed").is_some());
        let shown = if compact {
            // The phone lists every active row of a project.
            8
        } else {
            verify(cx, expected(6, false, false));
            click(cx, "show-more-many");
            8
        };
        verify(cx, expected(shown, false, false));
        click(cx, "settled-lead");
        verify(cx, expected(shown, true, false));
        if compact {
            // A shelf row shows when it settled, an active row its last update.
            let shown_times = sidebar.update(cx, |sidebar, cx| {
                sidebar
                    .compact_model(None, cx)
                    .rows
                    .iter()
                    .filter_map(|row| match row {
                        CompactListRow::Thread(row)
                            if ["lead-settled-1", "mixed-a"].contains(&row.meta.id.as_str()) =>
                        {
                            Some((row.meta.id.clone(), row.timestamp))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            });
            assert_eq!(
                shown_times,
                [("lead-settled-1".into(), 70), ("mixed-a".into(), 300)]
            );
        }
        click(cx, "settled-lead");
        verify(cx, expected(shown, false, false));
        // The shell behind shows the thread from a store of its own; this
        // one opens it as the pane showing it would, which reads it from
        // the host, and its shelf opens on what the host says it holds.
        store.update(cx, |store, cx| {
            store.show_thread("many-settled-1".into(), cx);
            store.open_thread("many-settled-1", cx);
        });
        for _ in 0..2 {
            cx.run_until_parked();
            crate::test_frames::draw_settled(cx);
        }
        verify(cx, expected(shown, false, true));

        send(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                collapsed_projects: vec!["mixed".into()],
                ..Default::default()
            }),
        );
        cx.run_until_parked();
        cx.run_until_parked();
        crate::test_frames::draw_settled(cx);
        assert_eq!(
            sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx)),
            lead(false)
                .into_iter()
                .chain(many(8, true))
                .collect::<Vec<_>>(),
            "a collapsed project lists nothing; compact={compact}"
        );
        send(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        );
        cx.run_until_parked();
        cx.run_until_parked();
    }
}

/// A thread pinned from its menu while the list is scrolled down leads
/// Pinned, and the list shows it as soon as the host's change arrives,
/// on the desktop and on the phone. A row added above a scrolled list
/// leaves the row at its top in place.
#[gpui::test]
fn a_thread_pinned_from_a_scrolled_list_leads_pinned_on_screen(cx: &mut TestAppContext) {
    use tcode_protocol::{
        ClientPayload, CommandResponse, EventEnvelope, HostMessage, IndexSnapshot, ServerEvent,
        Topic, decode_client_line, encode_line,
    };
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    cx.update(crate::test_client::init);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let projects = ["alpha", "beta"].map(|id| {
        let mut project = Project::from_root(PathBuf::from(format!("/{id}")));
        project.id = id.into();
        project
    });
    let mut sessions: Vec<_> = (0..60)
        .map(|index| {
            let mut meta = session(&format!("thread-{index}"), None);
            meta.project_id = Some(projects[index as usize % 2].id.clone());
            meta.created_at = 1_000 - index;
            meta.updated_at = meta.created_at;
            meta
        })
        .collect();
    for (meta, key) in sessions.iter_mut().zip(["h", "n"]) {
        meta.pinned_at = Some(1);
        meta.pin_order = Some(key.into());
    }
    let snapshot = |sessions: &Vec<SessionMeta>| {
        encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(IndexSnapshot {
                sessions: sessions.clone(),
                projects: projects.to_vec(),
                summary: Default::default(),
            }),
        }))
        .unwrap()
    };
    incoming
        .try_send(
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::Settings,
                event: ServerEvent::SettingsSnapshot(Default::default()),
            }))
            .unwrap(),
        )
        .unwrap();
    incoming.try_send(snapshot(&sessions)).unwrap();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    // The host side answers every command and records the pins.
    let mut driver = crate::store::tests::LocalHostDriver::from_link(link);
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let host_recorded = recorded.clone();
    let host_incoming = incoming.clone();
    std::thread::spawn(move || {
        while let Ok(line) = outgoing.recv_blocking() {
            let message = decode_client_line(&line).unwrap();
            if let ClientPayload::Command(command) = message.payload {
                if let Command::PinSession {
                    session_id,
                    order_key,
                } = command
                {
                    host_recorded.lock().unwrap().push((session_id, order_key));
                }
                let ack = HostMessage::Ack {
                    id: message.id,
                    result: Ok(CommandResponse::Unit),
                };
                let _ = host_incoming.send_blocking(encode_line(&ack).unwrap());
            }
        }
    });
    driver.wait_until(cx, "the threads", |cx| {
        store.read_with(cx, |store, _| !store.flat_sessions().is_empty())
    });
    let window_state = cx.new(|_| WindowState::default());
    let mut sidebar = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx));
        sidebar = Some(view.clone());
        gpui_base::Root::new(view, window, cx)
    });
    let sidebar = sidebar.unwrap();
    cx.simulate_resize(size(px(393.), px(852.)));
    draw(cx);

    for (compact, id) in [(false, "thread-30"), (true, "thread-40")] {
        window_state.update(cx, |state, cx| {
            state.compact = compact;
            cx.notify();
        });
        sidebar.update(cx, |sidebar, _| sidebar.compact_model_dirty = true);
        draw(cx);
        let selector: &'static str = if compact {
            format!("compact-row-{id}")
        } else {
            format!("sidebar-thread-{id}")
        }
        .leak();
        let list = sidebar.read_with(cx, |sidebar, _| {
            if compact {
                sidebar.compact_list_state.clone()
            } else {
                sidebar.flat_list_state.clone()
            }
        });
        let target = if compact {
            sidebar.update(cx, |sidebar, cx| {
                sidebar
                    .compact_model(None, cx)
                    .rows
                    .iter()
                    .position(|row| matches!(row, CompactListRow::Thread(row) if row.meta.id == id))
                    .expect("target thread")
            })
        } else {
            28
        };
        list.scroll_to(gpui::ListOffset {
            item_ix: target,
            offset_in_item: px(0.),
        });
        sidebar.update(cx, |_, cx| cx.notify());
        draw(cx);
        assert!(cx.debug_bounds(selector).is_some(), "{id} is on screen");

        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.on_pin(&ThreadPin(local(id)), window, cx)
        });
        driver.wait_until(cx, "pin command reaches the host", |_| {
            !recorded.lock().unwrap().is_empty()
        });
        let pins = std::mem::take(&mut *recorded.lock().unwrap());
        let first = store.read_with(cx, |store, _| store.flat_sessions()[0].pin_order.clone());
        let [(pinned, Some(key))] = pins.as_slice() else {
            panic!("{pins:?}");
        };
        assert_eq!(pinned, id);
        assert!(key.as_str() < first.unwrap().as_str(), "{key} leads Pinned");
        let meta = sessions.iter_mut().find(|meta| meta.id == id).unwrap();
        meta.pinned_at = Some(2);
        meta.pin_order = Some(key.clone());
        incoming.try_send(snapshot(&sessions)).unwrap();
        driver.wait_until(cx, "the pin", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .thread_meta(id)
                    .is_some_and(|meta| meta.pinned_at.is_some())
            })
        });
        draw(cx);

        let ids = sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx));
        assert_eq!(ids[0], id, "compact={compact}");
        let row = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the pinned row is drawn; compact={compact}"));
        let viewport = list.viewport_bounds();
        assert!(
            row.top() >= viewport.top() && row.bottom() <= viewport.bottom(),
            "the pinned row {row:?} lies inside the list {viewport:?}; compact={compact}"
        );
    }

    // A thread started elsewhere adds a row above a scrolled list; the
    // row at the top of the viewport stays there.
    window_state.update(cx, |state, cx| {
        state.compact = false;
        cx.notify();
    });
    draw(cx);
    let list = sidebar.read_with(cx, |sidebar, _| sidebar.flat_list_state.clone());
    list.scroll_to(gpui::ListOffset {
        item_ix: 28,
        offset_in_item: px(0.),
    });
    sidebar.update(cx, |_, cx| cx.notify());
    draw(cx);
    let anchor = sidebar.read_with(cx, |sidebar, _| sidebar.flat_row_keys[28].clone());
    let mut started = session("started-elsewhere", None);
    started.project_id = Some("alpha".into());
    started.created_at = 2_000;
    started.updated_at = 2_000;
    sessions.push(started);
    incoming.try_send(snapshot(&sessions)).unwrap();
    driver.wait_until(cx, "the new thread", |cx| {
        store.read_with(cx, |store, _| {
            store.thread_meta("started-elsewhere").is_some()
        })
    });
    draw(cx);
    sidebar.read_with(cx, |sidebar, _| {
        let top = sidebar.flat_list_state.logical_scroll_top().item_ix;
        assert_eq!(sidebar.flat_row_keys[top], anchor);
    });
}

/// Pinned rows lead in key order with keyless pins after them, and Active
/// rows follow by their last activity, newest first. Dragging an active
/// row over another active one moves nothing: its rank places it. Dragging
/// it between two pins pins it with a key between theirs; Escape
/// cancels a drag; Move down writes only the moved key; unpinning or
/// settling a pinned thread undoes with its old key.
#[gpui::test]
fn pinned_rows_lead_and_a_drag_or_undo_writes_their_keys(cx: &mut TestAppContext) {
    use tcode_protocol::{
        ClientPayload, CommandResponse, EventEnvelope, HostMessage, IndexSnapshot, ServerEvent,
        Topic, decode_client_line, encode_line,
    };
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    cx.update(crate::test_client::init);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let mut project = Project::from_root(PathBuf::from("/sample"));
    project.id = "sample".into();
    let thread = |id: &str, created: u64| {
        let mut meta = session(id, None);
        meta.project_id = Some("sample".into());
        meta.created_at = created;
        meta.updated_at = created;
        meta
    };
    let pinned = |id: &str, created: u64, key: Option<&str>| {
        let mut meta = thread(id, created);
        meta.pinned_at = Some(created);
        meta.pin_order = key.map(str::to_owned);
        meta
    };
    let mut reopened = thread("reopened", 10);
    reopened.unsettled_at = Some(99);
    let mut busy = thread("busy", 50);
    busy.updated_at = 1_000;
    let mut settled = thread("settled", 60);
    settled.settled_at = Some(70);
    let sessions = vec![
        settled,
        thread("shown", 100),
        busy,
        reopened,
        thread("fresh", 95),
        pinned("pin-old", 80, None),
        pinned("pin-new", 90, None),
        pinned("pin-c", 3, Some("w")),
        pinned("pin-b", 1, Some("t")),
        pinned("pin-a", 2, Some("f")),
    ];
    for (topic, event) in [
        (
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        ),
        (
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                sessions,
                projects: vec![project],
                summary: Default::default(),
            }),
        ),
    ] {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    store.update(cx, |store, cx| store.show_alone("shown".into(), cx));
    let mut driver = crate::store::tests::LocalHostDriver::from_link(link);
    // The host records commands on its own thread; the driver routes replies
    // while the UI awaits each acknowledgement, as in the running app.
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let host_recorded = recorded.clone();
    let host_incoming = incoming.clone();
    std::thread::spawn(move || {
        while let Ok(line) = outgoing.recv_blocking() {
            let message = decode_client_line(&line).unwrap();
            if matches!(
                message.payload,
                ClientPayload::Query(tcode_protocol::Query::Ping)
            ) {
                let reply = HostMessage::QueryResult {
                    id: message.id,
                    result: Ok(tcode_protocol::QueryResponse::Pong),
                };
                let _ = host_incoming.send_blocking(encode_line(&reply).unwrap());
            }
            if let ClientPayload::Command(command) = message.payload {
                if matches!(
                    command,
                    Command::PinSession { .. }
                        | Command::UnpinSession { .. }
                        | Command::ReorderPinned { .. }
                        | Command::SettleSession { .. }
                        | Command::UnsettleSession { .. }
                ) {
                    host_recorded.lock().unwrap().push(command);
                }
                let ack = HostMessage::Ack {
                    id: message.id,
                    result: Ok(CommandResponse::Unit),
                };
                let _ = host_incoming.send_blocking(encode_line(&ack).unwrap());
            }
        }
    });
    driver.wait_until(cx, "the threads", |cx| {
        store.read_with(cx, |store, _| !store.flat_sessions().is_empty())
    });
    let window_state = cx.new(|_| WindowState::default());
    let mut sidebar = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx));
        sidebar = Some(view.clone());
        gpui_base::Root::new(view, window, cx)
    });
    let sidebar = sidebar.unwrap();
    cx.simulate_resize(size(px(393.), px(2000.)));
    // The lifecycle commands accepted since the last call.
    let sent = |cx: &mut VisualTestContext,
                driver: &mut crate::store::tests::LocalHostDriver,
                expected: usize| {
        driver.wait_until(cx, "lifecycle commands reach the host", |_| {
            recorded.lock().unwrap().len() >= expected
        });
        // A FIFO reply also establishes absence after a cancelled gesture.
        let link = driver.link.clone();
        let mut barrier = std::pin::pin!(link.query(tcode_protocol::Query::Ping));
        driver.wait_until(cx, "lifecycle command barrier", |_| {
            let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
            match std::future::Future::poll(barrier.as_mut(), &mut task_cx) {
                std::task::Poll::Ready(result) => {
                    result.expect("host barrier reply");
                    true
                }
                std::task::Poll::Pending => false,
            }
        });
        std::mem::take(&mut *recorded.lock().unwrap())
    };
    sent(cx, &mut driver, 0);
    draw(cx);
    let order = [
        "pin-a", "pin-b", "pin-c", "pin-new", "pin-old", "busy", "shown", "reopened", "fresh",
    ];
    for compact in [false, true] {
        window_state.update(cx, |state, cx| {
            state.compact = compact;
            cx.notify();
        });
        sidebar.update(cx, |sidebar, _| sidebar.compact_model_dirty = true);
        draw(cx);
        let ids = sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx));
        assert_eq!(ids, order, "compact={compact}");
        let mut previous = None;
        for id in order {
            let selector = if compact {
                format!("compact-row-{id}")
            } else {
                format!("sidebar-thread-{id}")
            };
            let top = cx.debug_bounds(selector.leak()).unwrap().top();
            assert!(previous.is_none_or(|previous| top > previous), "{id}");
            previous = Some(top);
        }
    }
    window_state.update(cx, |state, cx| {
        state.compact = false;
        cx.notify();
    });
    draw(cx);

    let left = gpui::MouseButton::Left;
    let none = gpui::Modifiers::default();
    let row = |cx: &mut VisualTestContext, id: &str| {
        cx.debug_bounds(format!("sidebar-thread-{id}").leak())
            .unwrap()
    };
    let start = row(cx, "fresh").center();
    cx.simulate_mouse_down(start, left, none);
    cx.simulate_mouse_move(start + gpui::point(px(0.), px(8.)), left, none);
    draw(cx);
    let target = row(cx, "busy");
    let over = gpui::point(target.center().x, target.top() + px(4.));
    cx.simulate_mouse_move(over, left, none);
    draw(cx);
    cx.simulate_mouse_up(over, left, none);
    draw(cx);
    assert_eq!(
        sent(cx, &mut driver, 0),
        vec![],
        "Active is not arranged by hand"
    );

    let start = row(cx, "fresh").center();
    cx.simulate_mouse_down(start, left, none);
    cx.simulate_mouse_move(start + gpui::point(px(0.), px(8.)), left, none);
    draw(cx);
    let target = row(cx, "pin-b");
    let over = gpui::point(target.center().x, target.top() + px(4.));
    cx.simulate_mouse_move(over, left, none);
    draw(cx);
    cx.simulate_mouse_up(over, left, none);
    let commands = sent(cx, &mut driver, 1);
    let [
        Command::PinSession {
            session_id,
            order_key: Some(key),
        },
    ] = commands.as_slice()
    else {
        panic!("{commands:?}");
    };
    assert_eq!(session_id, "fresh");
    assert!("f" < key.as_str() && key.as_str() < "t", "{key}");

    let start = row(cx, "busy").center();
    cx.simulate_mouse_down(start, left, none);
    cx.simulate_mouse_move(start + gpui::point(px(0.), px(8.)), left, none);
    draw(cx);
    let target = row(cx, "pin-a");
    cx.simulate_mouse_move(target.center(), left, none);
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.simulate_mouse_up(target.center(), left, none);
    draw(cx);
    assert_eq!(sent(cx, &mut driver, 0), vec![], "Escape cancels the drag");

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_unpin(&ThreadUnpin(local("pin-a")), window, cx)
    });
    assert_eq!(
        sent(cx, &mut driver, 1),
        vec![Command::UnpinSession {
            session_id: "pin-a".into()
        }]
    );
    sidebar.update_in(cx, |sidebar, window, cx| sidebar.undo_lifecycle(window, cx));
    assert_eq!(
        sent(cx, &mut driver, 1),
        vec![Command::PinSession {
            session_id: "pin-a".into(),
            order_key: Some("f".into()),
        }]
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_move(&ThreadMove(local("pin-a"), true), window, cx)
    });
    let commands = sent(cx, &mut driver, 1);
    let [
        Command::ReorderPinned {
            session_id,
            order_key,
        },
    ] = commands.as_slice()
    else {
        panic!("{commands:?}");
    };
    assert_eq!(session_id, "pin-a");
    assert!(
        "t" < order_key.as_str() && order_key.as_str() < "w",
        "{order_key}"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.settle_thread(&local("pin-b"), window, cx)
    });
    assert_eq!(
        sent(cx, &mut driver, 1),
        vec![Command::SettleSession {
            session_id: "pin-b".into()
        }]
    );
    sidebar.update_in(cx, |sidebar, window, cx| sidebar.undo_lifecycle(window, cx));
    assert_eq!(
        sent(cx, &mut driver, 2),
        vec![
            Command::UnsettleSession {
                session_id: "pin-b".into()
            },
            Command::PinSession {
                session_id: "pin-b".into(),
                order_key: Some("t".into()),
            },
        ]
    );
    driver.link.close();
}

/// A machine that lets this device only view it says so beside its name
/// and its project offers no new thread; the other machine in the same
/// list keeps its own.
#[gpui::test]
fn a_machine_that_lets_this_device_only_view_it_starts_no_thread(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    cx.update(crate::test_client::init);
    cx.update(|cx| DevicePreferences::set_sidebar_layout(SidebarLayout::Grouped, cx));
    let (a, b) = (
        TestHost::viewed(cx, "host-a", &[("a-thread", 2)]),
        TestHost::remote(cx, "host-b", &[("b-thread", 1)]),
    );
    let window_state = cx.new(|_| WindowState::default());
    let stores = vec![a.store.clone(), b.store.clone()];
    let sidebar = cx.new(|cx| SessionsSidebar::new(stores, window_state, cx));
    let (_, cx) =
        cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar.clone(), window, cx));
    cx.simulate_resize(size(px(320.), px(800.)));
    crate::test_frames::draw_settled(cx);
    let project = |host: &TestHost, cx: &mut VisualTestContext| {
        host.store
            .read_with(cx, |store, _| store.projects()[0].id.clone())
    };
    let (project_a, project_b) = (project(&a, cx), project(&b, cx));
    let drawn =
        |selector: String, cx: &mut VisualTestContext| cx.debug_bounds(selector.leak()).is_some();
    assert!(drawn(format!("project-header-host-a/{project_a}"), cx));
    assert!(
        !drawn(format!("new-thread-host-a/{project_a}"), cx),
        "no thread starts on the machine that lets this device only view it"
    );
    assert!(drawn("host-view-only-host-a".into(), cx));
    assert!(
        drawn(format!("new-thread-host-b/{project_b}"), cx),
        "the other machine keeps its own"
    );
    assert!(!drawn("host-view-only-host-b".into(), cx));
    b.shutdown(cx);
    // The viewer may not stop its machine: the host goes with its channels.
    a.driver.link.close();
    a.host.to_host.close();
    let _ = std::fs::remove_dir_all(a.root);
}

/// Over two machines the project view lists each under its heading,
/// host › project › thread. A thread id both hosts use is a row under
/// each, and choosing one shows it on its own host; a heading shuts its
/// own host's section alone, on the desktop and the phone alike.
#[gpui::test]
fn two_hosts_list_their_projects_and_threads_under_their_headings(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let (_, window_state, a, b, cx) = sidebar_over_two_hosts(
        cx,
        SidebarLayout::Grouped,
        &[("shared", 300), ("a-only", 200)],
        &[("shared", 250), ("b-only", 100)],
        800.,
    );
    let project = |host: &TestHost, cx: &mut VisualTestContext| {
        host.store
            .read_with(cx, |store, _| store.projects()[0].id.clone())
    };
    let (project_a, project_b) = (project(&a, cx), project(&b, cx));
    let drawn =
        |selector: &str, cx: &mut VisualTestContext| cx.debug_bounds(selector.to_owned().leak());
    let order = [
        "host-heading-host-a".to_owned(),
        format!("project-header-host-a/{project_a}"),
        "sidebar-thread-host-a/shared".into(),
        "sidebar-thread-host-a/a-only".into(),
        "host-heading-host-b".into(),
        format!("project-header-host-b/{project_b}"),
        "sidebar-thread-host-b/shared".into(),
        "sidebar-thread-host-b/b-only".into(),
    ];
    let tops: Vec<_> = order
        .iter()
        .map(|selector| {
            drawn(selector, cx)
                .unwrap_or_else(|| panic!("{selector} is drawn"))
                .top()
        })
        .collect();
    assert!(
        tops.windows(2).all(|pair| pair[0] < pair[1]),
        "{order:?} lie in that order: {tops:?}"
    );

    let shown = Rc::new(std::cell::RefCell::new(Vec::new()));
    for (host, store) in [("host-a", &a.store), ("host-b", &b.store)] {
        let shown = shown.clone();
        cx.update(|_, cx| {
            cx.subscribe(store, move |_, event: &crate::store::ShowThread, _| {
                shown.borrow_mut().push((host, event.session_id.clone()));
            })
            .detach()
        });
    }
    let row = drawn("sidebar-thread-host-b/shared", cx).unwrap();
    cx.simulate_click(row.center(), gpui::Modifiers::default());
    draw(cx);
    assert_eq!(*shown.borrow(), [("host-b", "shared".to_owned())]);

    let heading = drawn("host-heading-host-a", cx).unwrap();
    cx.simulate_click(heading.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    for selector in &order[1..4] {
        assert!(drawn(selector, cx).is_none(), "{selector} folds away");
    }
    for selector in order.iter().take(1).chain(&order[4..]) {
        assert!(drawn(selector, cx).is_some(), "{selector} stays");
    }

    // The phone's list has the same sections, and the same one is shut.
    window_state.update(cx, |state, cx| state.set_compact(true, cx));
    crate::test_frames::draw_settled(cx);
    let compact = [
        "compact-host-host-a",
        "compact-host-host-b",
        "compact-row-host-b/shared",
        "compact-row-host-b/b-only",
    ];
    let tops: Vec<_> = compact
        .iter()
        .map(|selector| {
            drawn(selector, cx)
                .unwrap_or_else(|| panic!("{selector} is drawn"))
                .top()
        })
        .collect();
    assert!(
        tops.windows(2).all(|pair| pair[0] < pair[1]),
        "{compact:?} lie in that order: {tops:?}"
    );
    assert!(drawn("compact-row-host-a/shared", cx).is_none());
    let heading = drawn("compact-host-host-a", cx).unwrap();
    cx.simulate_click(heading.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    let opened = drawn("compact-row-host-a/shared", cx).expect("host A's section opens");
    assert!(heading.top() < opened.top() && opened.top() < drawn(compact[1], cx).unwrap().top());

    a.shutdown(cx);
    b.shutdown(cx);
}

/// Over two machines the threads view lists their threads by time, each
/// row naming its host. A pinned thread carried over another host's pinned
/// row lands among its own host's rows, after those above that row; only
/// its host is asked to move it, and the list keeps each host's order
/// after.
#[gpui::test]
fn a_thread_carried_over_another_hosts_row_lands_among_its_own(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let (_, _, mut a, mut b, cx) = sidebar_over_two_hosts(
        cx,
        SidebarLayout::Flat,
        &[("a1", 300), ("a2", 250), ("a3", 100)],
        &[("b1", 280), ("b2", 200)],
        800.,
    );
    let row = |id: &str, cx: &mut VisualTestContext| {
        cx.debug_bounds(format!("sidebar-thread-{id}").leak())
            .unwrap_or_else(|| panic!("{id} is drawn"))
    };
    let drawn = |cx: &mut VisualTestContext| {
        let mut rows: Vec<_> = [
            "host-a/a1",
            "host-a/a2",
            "host-a/a3",
            "host-b/b1",
            "host-b/b2",
        ]
        .into_iter()
        .map(|id| (row(id, cx).top(), id))
        .collect();
        rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        rows.into_iter().map(|(_, id)| id).collect::<Vec<_>>()
    };
    assert_eq!(
        drawn(cx),
        [
            "host-a/a1",
            "host-b/b1",
            "host-a/a2",
            "host-b/b2",
            "host-a/a3"
        ]
    );
    assert!(
        cx.debug_bounds("flat-host-host-b/b1").is_some(),
        "a row names its host"
    );

    // Pinned is the section arranged by hand: each host pins its threads
    // in the order it lists them.
    for (host, ids) in [(&mut a, &["a1", "a2", "a3"][..]), (&mut b, &["b1", "b2"])] {
        for (id, key) in ids.iter().zip(["f", "n", "t"]) {
            host.driver.command(
                cx,
                Command::PinSession {
                    session_id: (*id).into(),
                    order_key: Some(key.into()),
                },
            );
        }
        let store = host.store.clone();
        host.driver.wait_until(cx, "the pins", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .flat_sessions()
                    .iter()
                    .all(|meta| meta.pinned_at.is_some())
            })
        });
    }
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        drawn(cx),
        [
            "host-a/a1",
            "host-b/b1",
            "host-a/a2",
            "host-b/b2",
            "host-a/a3"
        ],
        "the pinned rows of two hosts merge by time"
    );

    let (left, none) = (gpui::MouseButton::Left, gpui::Modifiers::default());
    let frame = |cx: &mut VisualTestContext| {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(16));
        cx.update(|window, cx| window.simulate_next_frame(cx));
        draw(cx);
    };
    let (from, over) = (row("host-a/a3", cx), row("host-b/b1", cx));
    cx.simulate_mouse_down(from.center(), left, none);
    cx.simulate_mouse_move(from.center() + gpui::point(px(0.), px(8.)), left, none);
    draw(cx);
    let to = gpui::point(over.center().x, over.top() + px(4.));
    cx.simulate_mouse_move(to, left, none);
    for _ in 0..20 {
        frame(cx);
    }
    cx.simulate_mouse_up(to, left, none);
    draw(cx);
    let store = a.store.clone();
    a.driver.wait_until(cx, "the move", |cx| {
        store.read_with(cx, |store, _| {
            store
                .flat_sessions()
                .iter()
                .map(|meta| meta.id.as_str())
                .eq(["a1", "a3", "a2"])
        })
    });
    b.driver.step(cx);
    assert_eq!(b.listed(cx), ["b1", "b2"]);
    assert!(
        b.store.read_with(cx, |store, _| store
            .flat_sessions()
            .iter()
            .map(|meta| meta.pin_order.as_deref())
            .eq([Some("f"), Some("n")])),
        "the other host is not asked to move anything"
    );
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        drawn(cx),
        [
            "host-a/a1",
            "host-b/b1",
            "host-b/b2",
            "host-a/a3",
            "host-a/a2"
        ],
        "host A's arranged order holds among host B's rows by time"
    );

    a.shutdown(cx);
    b.shutdown(cx);
}

/// One host's link going down leaves the other host's rows alone: their
/// keys and where the list is scrolled stay. The host that went shows
/// its heading and how its link is in place of its threads, and Pair
/// again once it refused this device.
#[gpui::test]
fn a_host_going_down_leaves_the_other_hosts_rows_and_scroll(cx: &mut TestAppContext) {
    use tcode_client::{ConnectionFailure, ConnectionState};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    cx.update(|cx| cx.set_reduce_motion(true));
    let a: Vec<(String, u64)> = (0..30)
        .map(|index| (format!("a-{index:02}"), 1_000 - 2 * index))
        .collect();
    let b: Vec<(String, u64)> = (0..5)
        .map(|index| (format!("b-{index}"), 999 - 2 * index))
        .collect();
    fn refs(threads: &[(String, u64)]) -> Vec<(&str, u64)> {
        threads.iter().map(|(id, at)| (id.as_str(), *at)).collect()
    }
    let (sidebar, window_state, a, b, cx) =
        sidebar_over_two_hosts(cx, SidebarLayout::Flat, &refs(&a), &refs(&b), 400.);
    let list = sidebar.read_with(cx, |sidebar, _| sidebar.flat_list_state.clone());
    let keys = |cx: &mut VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| sidebar.flat_row_keys.clone())
    };
    let scroll_to = |item_ix, cx: &mut VisualTestContext| {
        list.scroll_to(gpui::ListOffset {
            item_ix,
            offset_in_item: px(10.),
        });
        sidebar.update(cx, |_, cx| cx.notify());
        draw(cx);
    };
    fn host_a(keys: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
        keys.iter()
            .filter(|(_, id)| id.starts_with("host-a/"))
            .cloned()
            .collect()
    }
    for (state, refused) in [
        (
            ConnectionState::Reconnecting {
                attempt: 1,
                reason: Some(ConnectionFailure::Unreachable(None)),
            },
            false,
        ),
        (
            ConnectionState::Offline {
                reason: ConnectionFailure::AuthenticationRejected,
            },
            true,
        ),
    ] {
        let before = keys(cx);
        let anchor = before
            .iter()
            .position(|key| *key == ("thread", "host-a/a-20".to_owned()))
            .expect("a-20 is listed");
        scroll_to(anchor, cx);
        let offset = list.logical_scroll_top();
        let top = cx
            .debug_bounds("sidebar-thread-host-a/a-20")
            .expect("a-20 is drawn")
            .top();

        b.driver.link.set_connection_state(state.clone());
        cx.run_until_parked();
        draw(cx);
        let after = keys(cx);
        assert_eq!(host_a(&after), host_a(&before), "{state:?}");
        assert_eq!(
            after[list.logical_scroll_top().item_ix],
            ("thread", "host-a/a-20".to_owned()),
            "{state:?}"
        );
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            offset.offset_in_item
        );
        assert_eq!(
            cx.debug_bounds("sidebar-thread-host-a/a-20")
                .map(|row| row.top()),
            Some(top),
            "{state:?}"
        );
        assert!(
            !after.iter().any(|(_, id)| id.starts_with("host-b/")),
            "{state:?} lists none of its threads"
        );
        assert_eq!(after[0], ("host", "host-b".to_owned()));

        list.scroll_to(gpui::ListOffset::default());
        sidebar.update(cx, |_, cx| cx.notify());
        draw(cx);
        assert!(cx.debug_bounds("host-heading-host-b").is_some());
        assert!(cx.debug_bounds("host-status-host-b").is_some());
        assert_eq!(
            cx.debug_bounds("host-pair-again-host-b").is_some(),
            refused,
            "{state:?}"
        );
    }
    let pair = cx.debug_bounds("host-pair-again-host-b").unwrap();
    cx.simulate_click(pair.center(), gpui::Modifiers::default());
    draw(cx);
    assert_eq!(
        window_state.read_with(cx, |state, _| state.destination()),
        Destination::Pair
    );

    a.shutdown(cx);
    b.shutdown(cx);
}
