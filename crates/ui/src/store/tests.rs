use agent::{AgentEvent, ItemContent, ProviderKind, ThreadItem, TurnStatus};
use gpui::{App, AppContext as _, TestAppContext};
use tcode_core::{
    project::{Project, SessionMeta},
    session::{EntryContent, ReviewComment, ReviewSide, StoredEvent},
    settings::Settings,
};
use tcode_protocol::{Command, EventEnvelope, ServerEvent, Subscription, Topic};
use tcode_runtime::pipe::{HostServices, SpawnedHost, spawn_host};
use tcode_services::store::SessionStore;

use super::{ConversationDestination, WorkspaceAttachment, WorkspaceStore, history::HeldHistory};

pub(crate) fn seed_full_scope(
    store: &gpui::Entity<WorkspaceStore>,
    incoming: &async_channel::Sender<String>,
    deferred: Vec<String>,
    cx: &mut TestAppContext,
) {
    seed_full_scope_reply(store, incoming, 1, deferred, cx);
}

pub(crate) fn seed_full_scope_reply(
    store: &gpui::Entity<WorkspaceStore>,
    incoming: &async_channel::Sender<String>,
    request_id: u64,
    deferred: Vec<String>,
    cx: &mut TestAppContext,
) {
    incoming
        .try_send(
            tcode_protocol::encode_line(&tcode_protocol::HostMessage::Event(EventEnvelope {
                request_id: Some(request_id),
                topic: Topic::Scope,
                event: ServerEvent::ScopeSnapshot {
                    scope: tcode_protocol::Scope::Full,
                    host_unix_ms: 0,
                },
            }))
            .unwrap(),
        )
        .unwrap();
    let link = store.read_with(cx, |store, _| store.host.clone());
    let mut pump = std::pin::pin!(link.pump());
    let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    cx.run_until_parked();
    for line in deferred {
        incoming.try_send(line).unwrap();
    }
}

struct ScriptedSpace {
    link: tcode_client::HostLink,
    scope: std::sync::Arc<std::sync::Mutex<tcode_protocol::Scope>>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<tcode_protocol::ClientMessage>>>,
    worker: Option<std::thread::JoinHandle<()>>,
    driver: LocalHostDriver,
}

impl ScriptedSpace {
    fn new() -> Self {
        use tcode_protocol::{ClientPayload, CommandResponse, HostMessage, Scope};
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let project = project_at("shared", std::path::Path::new("/shared"));
        let scope = std::sync::Arc::new(std::sync::Mutex::new(Scope::Space {
            space_id: "space-one".into(),
            space_name: "Team".into(),
            projects: vec![project],
            providers: vec![tcode_protocol::ScopedProviderChoice {
                provider: ProviderKind::Codex,
                profile_id: Some("team-agent".into()),
                name: "Team agent".into(),
                models: vec![agent::ModelSpec {
                    id: "team-model".into(),
                    display_name: "Team model".into(),
                    is_default: true,
                    options: Vec::new(),
                }],
            }],
            policy: tcode_protocol::SpacePolicy::unrestricted(),
        }));
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker_scope = scope.clone();
        let worker_recorded = recorded.clone();
        let worker = std::thread::spawn(move || {
            smol::block_on(async move {
                while let Ok(line) = requests.recv().await {
                    let request = tcode_protocol::decode_client_line(&line).unwrap();
                    worker_recorded.lock().unwrap().push(request.clone());
                    let scope = worker_scope.lock().unwrap().clone();
                    let response = match request.payload {
                        ClientPayload::Subscribe(subscription)
                            if matches!(
                                subscription.topic,
                                Topic::RuntimeEvents | Topic::Preview { .. }
                            ) && scope == Scope::Full =>
                        {
                            HostMessage::Ack {
                                id: request.id,
                                result: Ok(CommandResponse::Unit),
                            }
                        }
                        ClientPayload::Subscribe(subscription) => {
                            let event = match (&subscription.topic, &scope) {
                                (Topic::Scope, _) => ServerEvent::ScopeSnapshot {
                                    scope: scope.clone(),
                                    host_unix_ms: 0,
                                },
                                (
                                    Topic::SpaceIndex { space_id },
                                    Scope::Space {
                                        space_id: scoped_id,
                                        projects,
                                        ..
                                    },
                                ) if space_id == scoped_id => {
                                    let sessions = projects
                                        .first()
                                        .map(|project| {
                                            let mut session = thread(
                                                &project.root,
                                                "shared-thread",
                                                &project.id,
                                                None,
                                            );
                                            session.updated_at = 100;
                                            session
                                        })
                                        .into_iter()
                                        .collect();
                                    ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                                        projects: projects.clone(),
                                        sessions,
                                        summary: Default::default(),
                                    })
                                }
                                (Topic::Index, Scope::Full) => {
                                    ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                                        projects: vec![],
                                        sessions: vec![],
                                        summary: Default::default(),
                                    })
                                }
                                (Topic::Settings, Scope::Full) => {
                                    ServerEvent::SettingsSnapshot(Settings::default())
                                }
                                (Topic::Providers, Scope::Full) => ServerEvent::ProvidersReplaced(
                                    tcode_protocol::ProvidersStatus::default(),
                                ),
                                _ => {
                                    let rejection = HostMessage::Ack {
                                        id: request.id,
                                        result: Err(tcode_protocol::ProtocolError::out_of_scope(
                                            "subscription outside current space",
                                        )),
                                    };
                                    if replies
                                        .send(tcode_protocol::encode_line(&rejection).unwrap())
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                    continue;
                                }
                            };
                            HostMessage::Event(EventEnvelope {
                                request_id: Some(request.id),
                                topic: subscription.topic,
                                event,
                            })
                        }
                        ClientPayload::Command(Command::StartDraft { .. }) => HostMessage::Ack {
                            id: request.id,
                            result: Ok(CommandResponse::SessionId(None)),
                        },
                        ClientPayload::Command(_) => HostMessage::Ack {
                            id: request.id,
                            result: Ok(CommandResponse::Unit),
                        },
                        ClientPayload::Query(tcode_protocol::Query::Hosting { action }) => {
                            let created = matches!(
                                action,
                                tcode_protocol::HostingAction::Spaces(
                                    tcode_protocol::SpaceAction::Create { .. }
                                )
                            );
                            let shared = matches!(
                                action,
                                tcode_protocol::HostingAction::Spaces(
                                    tcode_protocol::SpaceAction::SetProjects { .. }
                                )
                            );
                            let spaces = ["created", "concurrent"]
                                .into_iter()
                                .map(|id| tcode_protocol::SpaceInfo {
                                    id: id.into(),
                                    name: id.into(),
                                    created_unix: 1,
                                    project_ids: if shared && id == "created" {
                                        vec!["shared".into()]
                                    } else {
                                        vec![]
                                    },
                                    link: None,
                                    link_enabled: true,
                                    link_dead: false,
                                    members: vec![],
                                    policy: tcode_protocol::SpacePolicy::unrestricted(),
                                    hidden_from_threads: false,
                                })
                                .collect();
                            HostMessage::QueryResult {
                                id: request.id,
                                result: Ok(tcode_protocol::QueryResponse::Hosting(
                                    tcode_protocol::HostingState {
                                        created_space_id: created.then(|| "created".into()),
                                        spaces: if matches!(
                                            action,
                                            tcode_protocol::HostingAction::State
                                        ) {
                                            vec![]
                                        } else {
                                            spaces
                                        },
                                        enabled: true,
                                        expires_in_secs: 0,
                                        host_id: "machine".into(),
                                        host_name: "Studio".into(),
                                        invite: None,
                                        devices: vec![],
                                        traverse: vec![],
                                        group: None,
                                    },
                                )),
                            }
                        }
                        ClientPayload::Query(tcode_protocol::Query::Ping) => {
                            HostMessage::QueryResult {
                                id: request.id,
                                result: Ok(tcode_protocol::QueryResponse::Pong),
                            }
                        }
                        ClientPayload::Query(tcode_protocol::Query::ReadProjectIcon { .. }) => {
                            HostMessage::QueryResult {
                                id: request.id,
                                result: Err(tcode_protocol::ProtocolError {
                                    code: "not_found".into(),
                                    message: "no project icon".into(),
                                }),
                            }
                        }
                        ClientPayload::Unsubscribe(_) => continue,
                        other => panic!("unexpected member request: {other:?}"),
                    };
                    let line = tcode_protocol::encode_line(&response).unwrap();
                    if replies.send(line).await.is_err() {
                        break;
                    }
                }
            });
        });
        Self {
            driver: LocalHostDriver::from_link(link.clone()),
            link,
            scope,
            requests: recorded,
            worker: Some(worker),
        }
    }

    fn store(&mut self, cx: &mut TestAppContext) -> gpui::Entity<WorkspaceStore> {
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                self.link.clone(),
                WorkspaceAttachment::Remote {
                    host_id: "machine".into(),
                    host_name: "Studio".into(),
                },
                None,
                None,
                cx,
            )
        });
        self.driver.wait_until(cx, "space baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        workspace
    }

    fn barrier(&mut self, cx: &mut TestAppContext) {
        self.driver
            .await_with_events(cx, self.link.query(tcode_protocol::Query::Ping))
            .unwrap();
    }
}

impl Drop for ScriptedSpace {
    fn drop(&mut self) {
        self.link.close();
        self.worker.take().unwrap().join().unwrap();
    }
}

/// The settings a member device keeps for its space cannot be read: the
/// user is told, the workspace runs on defaults, and a change made there
/// writes nothing over what the device saved.
#[gpui::test]
fn unreadable_member_settings_are_reported_and_never_written_over(cx: &mut TestAppContext) {
    use std::cell::RefCell;
    use tcode_client::host::{ClientHost, ClientPreferences, Transport};
    use tcode_client::pairing::PairedHost;
    struct Device(RefCell<ClientPreferences>);
    impl ClientHost for Device {
        fn device_name(&self) -> String {
            "member".into()
        }
        fn device_id(&self) -> Result<String, tcode_client::host::FileFailure> {
            Ok("member-device".into())
        }
        fn device_platform(&self) -> Option<String> {
            None
        }
        fn load_preferences(&self) -> Result<ClientPreferences, tcode_client::host::FileFailure> {
            Ok(self.0.borrow().clone())
        }
        fn save_preferences(
            &self,
            preferences: &ClientPreferences,
        ) -> Result<(), tcode_client::host::FileFailure> {
            *self.0.borrow_mut() = preferences.clone();
            Ok(())
        }
        fn load_hosts(&self) -> Result<Vec<PairedHost>, tcode_client::host::FileFailure> {
            Ok(Vec::new())
        }
        fn save_hosts(&self, _: &[PairedHost]) -> Result<(), tcode_client::host::FileFailure> {
            Ok(())
        }
        fn last_host_id(&self) -> Option<String> {
            None
        }
        fn set_last_host_id(&self, _: Option<&str>) {}
        fn connect(&self, _: &PairedHost) -> Transport {
            unreachable!("the store is handed its link")
        }
    }
    cx.update(crate::test_client::init);
    let saved = serde_json::json!("not this device's settings");
    let device = std::rc::Rc::new(Device(RefCell::new(ClientPreferences {
        navigation: Some(serde_json::json!({
            "member_settings": { "machine:space-one": saved.clone() }
        })),
        ..ClientPreferences::default()
    })));
    cx.update(|cx| crate::device::DevicePreferences::install(device.clone(), cx));
    let mut space = ScriptedSpace::new();
    let workspace = cx.new(|cx| {
        WorkspaceStore::new_attached(
            space.link.clone(),
            WorkspaceAttachment::Remote {
                host_id: "machine".into(),
                host_name: "Studio".into(),
            },
            Some(device.clone()),
            None,
            cx,
        )
    });
    let reported = std::rc::Rc::new(RefCell::new(Vec::new()));
    cx.update(|cx| {
        let reported = reported.clone();
        cx.subscribe(&workspace, move |_, event: &super::RuntimeEvent, _| {
            reported.borrow_mut().push(event.clone());
        })
        .detach();
    });
    space.driver.wait_until(cx, "space baseline", |cx| {
        workspace.read_with(cx, |store, _| store.baseline_ready())
    });
    assert!(matches!(
        reported.borrow().as_slice(),
        [super::RuntimeEvent::Error(
            tcode_protocol::RuntimeError::FileUnreadable { .. }
        )]
    ));
    workspace.update(cx, |store, cx| store.cycle_project_sort(cx));
    cx.run_until_parked();
    assert_ne!(
        workspace.read_with(cx, |store, _| store.project_sort()),
        Settings::default().project_sort
    );
    assert_eq!(
        device.0.borrow().navigation.as_ref().unwrap()["member_settings"]["machine:space-one"],
        saved
    );
}

/// A member's change to its space's settings is saved among the device's
/// preferences, and a later change of the device's own keeps it.
#[gpui::test]
fn member_settings_are_saved_among_the_device_preferences(cx: &mut TestAppContext) {
    use tcode_client::host::ClientHost as _;
    use tcode_core::settings::SidebarLayout;
    cx.update(crate::test_client::init);
    let root = scratch_root("member-settings-saved");
    std::fs::create_dir_all(&root).unwrap();
    let device = std::rc::Rc::new(tcode_traverse::NativeClientHost::new(
        root.clone(),
        "member",
    ));
    cx.update(|cx| crate::device::DevicePreferences::install(device.clone(), cx));
    let mut space = ScriptedSpace::new();
    let workspace = cx.new(|cx| {
        WorkspaceStore::new_attached(
            space.link.clone(),
            WorkspaceAttachment::Remote {
                host_id: "machine".into(),
                host_name: "Studio".into(),
            },
            Some(device.clone()),
            None,
            cx,
        )
    });
    space.driver.wait_until(cx, "space baseline", |cx| {
        workspace.read_with(cx, |store, _| store.baseline_ready())
    });
    workspace.update(cx, |store, cx| store.cycle_project_sort(cx));
    cx.run_until_parked();
    cx.update(|cx| {
        crate::device::DevicePreferences::set_sidebar_layout(SidebarLayout::Grouped, cx)
    });
    let sort = workspace.read_with(cx, |store, _| store.project_sort());
    assert_ne!(sort, Settings::default().project_sort);
    let saved = device.load_preferences().unwrap();
    assert_eq!(
        saved.navigation.unwrap()["member_settings"]["machine:space-one"]["project_sort"],
        serde_json::to_value(sort).unwrap()
    );
    assert_eq!(saved.sidebar_layout, Some(SidebarLayout::Grouped));
    let _ = std::fs::remove_dir_all(root);
}

/// Another account runs the CLI the default one does, so both pickers list
/// that CLI's catalog; a third-party endpoint answers to its card alone.
/// The host answers over a scripted transport.
#[gpui::test]
fn a_second_account_lists_the_cli_catalog_and_a_third_party_endpoint_only_its_card(
    cx: &mut TestAppContext,
) {
    use tcode_core::settings::{EnvVar, ProviderProfile, ProviderSettings};
    cx.update(crate::test_client::init);
    let (to_host, _outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let profile = |settings| ProviderProfile {
        kind: agent::ProviderKind::ClaudeCode,
        settings,
    };
    let mut settings = Settings::default();
    settings.profiles.extend([
        (
            "second-account".to_owned(),
            profile(ProviderSettings {
                home_path: Some("/profile-homes/second-account".into()),
                ..Default::default()
            }),
        ),
        (
            "kimi".to_owned(),
            profile(ProviderSettings {
                env: vec![EnvVar {
                    name: "ANTHROPIC_BASE_URL".into(),
                    value: "https://api.kimi.test/anthropic".into(),
                    sensitive: false,
                }],
                custom_models: vec!["kimi-model".into()],
                ..Default::default()
            }),
        ),
    ]);
    let providers = tcode_protocol::ProvidersStatus {
        model_catalogs: [(
            agent::ProviderKind::ClaudeCode,
            vec![agent::ModelSpec {
                id: "cli-model".into(),
                display_name: "CLI model".into(),
                is_default: true,
                options: Vec::new(),
            }],
        )]
        .into(),
        ..Default::default()
    };
    for (topic, event) in [
        (Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
        (Topic::Providers, ServerEvent::ProvidersReplaced(providers)),
    ] {
        incoming
            .try_send(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Event(EventEnvelope {
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
    let pump_link = link.clone();
    let _pump = cx.background_executor.spawn(async move {
        pump_link.pump().await;
    });
    let store =
        cx.new(|cx| WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx));
    seed_full_scope(&store, &incoming, deferred, cx);
    cx.run_until_parked();
    cx.run_until_parked();
    let ids = |store: &WorkspaceStore, profile: &str| {
        store
            .picker_models_for_profile(profile)
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>()
    };
    store.read_with(cx, |store, _| {
        assert_eq!(ids(store, "claude"), ["cli-model"]);
        assert_eq!(ids(store, "second-account"), ["cli-model"]);
        assert_eq!(ids(store, "kimi"), ["kimi-model"]);
    });
}

#[gpui::test]
fn member_scope_seeds_only_the_space_index_and_reseeds_on_reconnect(cx: &mut TestAppContext) {
    cx.update(crate::test_client::init);
    cx.update(|cx| cx.set_reduce_motion(true));
    let mut host = ScriptedSpace::new();
    let workspace = host.store(cx);
    host.driver.wait_until(cx, "member baseline", |cx| {
        workspace.read_with(cx, |store, _| store.baseline_ready())
    });
    host.barrier(cx);
    workspace.read_with(cx, |store, _| {
        assert_eq!(
            store
                .projects()
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["shared"]
        );
        assert_eq!(store.grouped_sessions()[0].sessions[0].id, "shared-thread");
        assert_eq!(store.enabled_profiles()[0].id, "team-agent");
        assert_eq!(
            store.picker_models_for_profile("team-agent")[0].id,
            "team-model"
        );
        assert!(store.settings_hydrated());
    });
    let subscriptions = host
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| match &request.payload {
            tcode_protocol::ClientPayload::Subscribe(subscription) => {
                Some(subscription.topic.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        subscriptions,
        [
            Topic::Scope,
            Topic::SpaceIndex {
                space_id: "space-one".into()
            }
        ]
    );
    cx.update(|cx| {
        crate::device::DevicePreferences::set_sidebar_layout(
            tcode_core::settings::SidebarLayout::Grouped,
            cx,
        )
    });
    let window_state = cx.new(|_| crate::window_state::WindowState::default());
    let (_sidebar, cx) = cx.add_window_view(|_, cx| {
        crate::sidebar::SessionsSidebar::new(vec![workspace.clone()], window_state, cx)
    });
    cx.simulate_resize(gpui::size(gpui::px(360.), gpui::px(800.)));
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert!(
        cx.debug_bounds("project-header-shared").is_some(),
        "the scoped project renders in the sidebar"
    );
    assert!(
        cx.debug_bounds("sidebar-thread-shared-thread").is_some(),
        "the scoped conversation renders in the sidebar"
    );
    *host.scope.lock().unwrap() = tcode_protocol::Scope::Space {
        space_id: "space-two".into(),
        space_name: "Moved".into(),
        projects: vec![project_at("other", std::path::Path::new("/other"))],
        providers: Vec::new(),
        policy: tcode_protocol::SpacePolicy::unrestricted(),
    };
    workspace.update(cx, |store, _| {
        let state = tcode_client::ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        };
        store.host.set_connection_state(state.clone());
        store.apply_connection_state(state);
        let state = tcode_client::ConnectionState::Syncing { path: None };
        store.host.set_connection_state(state.clone());
        store.apply_connection_state(state);
    });
    host.driver.wait_until(cx, "new space baseline", |cx| {
        workspace.read_with(cx, |store, _| {
            store.projects().first().is_some_and(|p| p.id == "other") && store.index_hydrated()
        })
    });
    host.barrier(cx);
    let requests = host.requests.lock().unwrap();
    let new_space = requests
        .iter()
        .find_map(|request| match &request.payload {
            tcode_protocol::ClientPayload::Subscribe(subscription)
                if subscription.topic
                    == (Topic::SpaceIndex {
                        space_id: "space-two".into(),
                    }) =>
            {
                Some(subscription)
            }
            _ => None,
        })
        .expect("new space index subscription");
    assert_eq!(
        new_space.known_index, None,
        "a new space needs its own baseline"
    );
    drop(requests);
    workspace.read_with(cx, |store, _| {
        assert!(store.enabled_profiles().is_empty());
        assert_eq!(
            store.index_topic(),
            Topic::SpaceIndex {
                space_id: "space-two".into()
            }
        );
    });
    assert!(
        host.requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| !matches!(
                &request.payload,
                tcode_protocol::ClientPayload::Subscribe(tcode_protocol::Subscription {
                    topic: Topic::Index
                        | Topic::Settings
                        | Topic::Providers
                        | Topic::RuntimeEvents
                        | Topic::Preview { .. }
                        | Topic::PreviewSession { .. }
                        | Topic::ExternalImport { .. },
                    ..
                })
            ))
    );
    assert!(
        cx.update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
            .is_none()
    );
    *host.scope.lock().unwrap() = tcode_protocol::Scope::Full;
    workspace.update(cx, |store, _| {
        let state = tcode_client::ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        };
        store.host.set_connection_state(state.clone());
        store.apply_connection_state(state);
        let state = tcode_client::ConnectionState::Syncing { path: None };
        store.host.set_connection_state(state.clone());
        store.apply_connection_state(state);
    });
    host.driver.wait_until(cx, "full scope baseline", |cx| {
        workspace.read_with(cx, |store, _| {
            store.scope().is_full() && store.baseline_ready()
        })
    });

    let spaces = cx
        .update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
        .expect("Full may manage its host without pair-time metadata");
    let created = spaces.update(cx, |spaces, cx| {
        spaces.create("Created".into(), Some("shared".into()), cx)
    });
    let result = std::rc::Rc::new(std::cell::RefCell::new(None));
    let captured = result.clone();
    cx.spawn(async move |_| {
        *captured.borrow_mut() = Some(created.await);
    })
    .detach();
    host.driver
        .wait_until(cx, "created space and project share", |_| {
            result.borrow().is_some()
        });
    assert_eq!(
        result.borrow().as_ref().unwrap().as_ref().unwrap().id,
        "created"
    );
    assert!(host.requests.lock().unwrap().iter().any(|request| matches!(&request.payload,
            tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Hosting {
                action: tcode_protocol::HostingAction::Spaces(tcode_protocol::SpaceAction::SetProjects { id, .. })
            }) if id == "created")));
    assert_eq!(
        spaces.read_with(cx, |spaces, _| spaces.sharing("shared")),
        ["created"]
    );
    *host.scope.lock().unwrap() = tcode_protocol::Scope::Space {
        space_id: "space-one".into(),
        space_name: "Current".into(),
        projects: vec![],
        providers: vec![],
        policy: tcode_protocol::SpacePolicy::unrestricted(),
    };
    workspace.update(cx, |store, cx| {
        let state = tcode_client::ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        };
        store.host.set_connection_state(state.clone());
        store.apply_connection_state(state);
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::Scope,
                event: ServerEvent::ScopeSnapshot {
                    scope: tcode_protocol::Scope::Full,
                    host_unix_ms: crate::time::now_millis(cx),
                },
            },
            cx,
        );
        assert!(
            !store.can_manage_host(),
            "an uncorrelated scope cannot establish a role while pending"
        );
        cx.notify();
    });
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
            .is_none()
    );
    assert!(
        spaces.read_with(cx, |spaces, _| spaces.spaces().is_empty()),
        "the cached share badges are invalidated"
    );
    workspace.update(cx, |store, _| {
        let state = tcode_client::ConnectionState::Syncing { path: None };
        store.host.set_connection_state(state.clone());
        store.apply_connection_state(state);
    });
    host.driver.wait_until(cx, "moved member baseline", |cx| {
        workspace.read_with(cx, |store, _| {
            !store.scope().is_full() && store.baseline_ready()
        })
    });
    workspace.update(cx, |store, cx| {
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::Scope,
                event: ServerEvent::ScopeSnapshot {
                    scope: tcode_protocol::Scope::Full,
                    host_unix_ms: crate::time::now_millis(cx),
                },
            },
            cx,
        );
        assert!(!store.scope().is_full());
        assert!(store.machine_label().contains("Current"));
    });
    assert!(
        cx.update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
            .is_none()
    );
}

#[gpui::test]
fn member_sidebar_and_read_preferences_change_without_host_commands(cx: &mut TestAppContext) {
    let mut host = ScriptedSpace::new();
    let workspace = host.store(cx);
    host.driver.wait_until(cx, "member baseline", |cx| {
        workspace.read_with(cx, |store, _| store.baseline_ready())
    });
    host.barrier(cx);
    host.requests.lock().unwrap().clear();
    workspace.update(cx, |store, cx| {
        store.toggle_project_collapsed("shared".into(), cx);
        store.cycle_project_sort(cx);
        store.toggle_favorite_model(agent::ProviderKind::Codex, None, "team-model".into(), cx);
        store.mark_session_unread("shared-thread".into(), cx);
        assert!(store.session_unread("shared-thread"));
        store.dispatch(Command::MarkSessionRead {
            session_id: "shared-thread".into(),
            through: 100,
        });
        assert!(!store.session_unread("shared-thread"));
        assert!(store.is_project_collapsed("shared"));
        assert_eq!(
            store.project_sort(),
            tcode_core::settings::ProjectSort::NameAsc
        );
        assert_eq!(
            store.settings().favorite_models,
            [tcode_core::settings::FavoriteModel::Row {
                provider: agent::ProviderKind::Codex,
                profile_id: None,
                model: "team-model".into(),
            }]
        );
    });
    host.barrier(cx);
    assert!(
        host.requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| !matches!(&request.payload, tcode_protocol::ClientPayload::Command(_)))
    );
}

#[cfg(preview_backend)]
#[gpui::test]
fn preview_reads_the_attachment_route_when_saved_hosts_are_stale(cx: &mut TestAppContext) {
    use std::rc::Rc;
    use tcode_client::host::{ClientHost as _, LiveHost};
    let root = crate::store::tests::scratch_root("preview-route");
    let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
    let mut host = tcode_client::pairing::PairedHost {
        host_id: "09e5c592f3234ef329634a0baa6bfe06193be6b64801121d84242af1d27dbecf".into(),
        name: "Machine".into(),
        traverse: Vec::new(),
        relay: None,
        addrs: vec!["192.168.31.5:47420".into()],
        last_connected_unix: None,
        group_id: None,
        space_id: None,
        space_name: None,
    };
    client.remember_host(host.clone()).unwrap();
    struct NoTunnels;
    impl tcode_client::host::TunnelOpener for NoTunnels {
        fn open(
            &self,
            _target: tcode_client::host::StreamTarget,
        ) -> tcode_client::host::TunnelFuture {
            Box::pin(async { Err(std::io::Error::from(std::io::ErrorKind::NotConnected)) })
        }
    }
    let current_host = LiveHost::with_tunnels(host.clone(), std::sync::Arc::new(NoTunnels));
    let (to_host, _outgoing) = async_channel::unbounded();
    let (_incoming, from_host) = async_channel::unbounded();
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            tcode_client::HostLink::new(to_host, from_host),
            WorkspaceAttachment::Remote {
                host_id: host.host_id.clone(),
                host_name: host.name.clone(),
            },
            Some(client.clone()),
            Some(current_host.clone()),
            cx,
        )
    });
    host.addrs = vec!["192.168.1.161:47420".into()];
    current_host.authenticated(&host);
    cx.update(|cx| assert_eq!(store.read(cx).preview_proxy().unwrap().unwrap().0, host));
    assert_eq!(
        client.load_hosts().unwrap()[0].addrs,
        ["192.168.31.5:47420"]
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(preview_backend)]
#[gpui::test]
fn preview_requests_for_an_unviewed_thread_reach_the_panel(cx: &mut TestAppContext) {
    cx.update(crate::test_client::init);
    cx.update(crate::markdown::init);
    let (to_host, _requests) = async_channel::unbounded();
    let (_replies, from_host) = async_channel::unbounded();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    let requests = store.read_with(cx, |store, _| store.remote_preview_requests());
    store.update(cx, |store, cx| {
        store.show_alone("viewed".into(), cx);
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::Preview {
                    client: tcode_protocol::PreviewClient {
                        id: "this-client".into(),
                        name: "This client".into(),
                        on_host: false,
                    },
                },
                event: ServerEvent::PreviewRequest {
                    request_id: 7,
                    session_id: "background".into(),
                    request: tcode_protocol::PreviewRequest::Status,
                },
            },
            cx,
        );
    });
    let forwarded = requests
        .try_recv()
        .expect("the panel answers for a background thread");
    assert!(matches!(
        forwarded.event,
        ServerEvent::PreviewRequest { request_id: 7, ref session_id, .. } if session_id == "background"
    ));
}

#[gpui::test]
fn scripted_host_send_waits_for_ack_and_rejection_offers_retry(cx: &mut TestAppContext) {
    cx.update(crate::test_client::init);
    cx.update(crate::markdown::init);
    let (to_host, requests) = async_channel::unbounded();
    let (replies, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
    // The next command the client sends, past the subscriptions that
    // opening and leaving threads send.
    let next_command = || loop {
        let request =
            tcode_protocol::decode_client_line(&requests.recv_blocking().unwrap()).unwrap();
        if matches!(request.payload, tcode_protocol::ClientPayload::Command(_)) {
            break request;
        }
    };
    store.update(cx, |store, cx| {
        store.show_alone("scripted".into(), cx);
        store.send_turn_for(
            store.sole_thread().unwrap_or_default(),
            "hello".into(),
            Vec::new(),
        );
    });
    let request = next_command();
    assert!(matches!(
        request.payload,
        tcode_protocol::ClientPayload::Command(Command::SendTurn { .. })
    ));
    let key = request.key.clone().unwrap();
    assert_eq!(
        store.read_with(cx, |store, cx| store
            .delivery_messages_for(store.sole_thread().as_deref(), cx)),
        vec![(key.clone(), "hello".into(), None, false)]
    );
    replies
        .send_blocking(
            tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                id: request.id,
                result: Err(tcode_protocol::ProtocolError {
                    code: "rejected".into(),
                    message: "Host refused this send".into(),
                }),
            })
            .unwrap(),
        )
        .unwrap();
    // Drive the production pump on the test thread; no mocked HostLink.
    let mut pump = std::pin::pin!(link.pump());
    let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    assert_eq!(
        store.read_with(cx, |store, cx| store
            .delivery_messages_for(store.sole_thread().as_deref(), cx)),
        vec![(
            key.clone(),
            "hello".into(),
            Some("Host refused this send".into()),
            false,
        )]
    );
    assert!(
        !store.read_with(cx, |store, cx| store
            .chat_loading_for(store.sole_thread().as_deref(), cx)),
        "a rejected write must be visible without a snapshot"
    );
    let window_state = cx.new(|_| crate::window_state::WindowState::default());
    let (_chat, visual) = crate::chat::tests::add_chat_window(cx, |window, cx| {
        crate::chat::ChatView::new(
            store.clone(),
            window_state,
            Some("scripted".to_owned()),
            window,
            cx,
        )
    });
    visual.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(700.)));
    visual.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert!(visual.debug_bounds("retry-delivery").is_some());
    assert!(visual.debug_bounds("discard-delivery").is_some());
    store.update(cx, |store, cx| store.retry_delivery(&key, cx));
    let retry = next_command();
    assert_ne!(retry.key, request.key);
    assert_eq!(retry.payload, request.payload);
    replies
        .send_blocking(
            tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                id: retry.id,
                result: Ok(tcode_protocol::CommandResponse::Unit),
            })
            .unwrap(),
        )
        .unwrap();
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    assert_eq!(
        store.read_with(cx, |store, cx| store
            .delivery_messages_for(store.sole_thread().as_deref(), cx)),
        vec![(retry.key.clone().unwrap(), "hello".into(), None, true)]
    );
    let accepted_key = retry.key.unwrap();
    let same_text_record = |id: String| {
        StoredEvent::from(AgentEvent::ItemCompleted(ThreadItem {
            id,
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: "hello".into(),
                attachments: Vec::new(),
            },
        }))
    };
    store.update(cx, |store, cx| {
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: "scripted".into(),
                },
                event: ServerEvent::SessionSnapshot {
                    from: 0,
                    end: 1,
                    total: 1,
                    total_turns: 1,
                    records: vec![same_text_record("unrelated".into())],
                    truncated: false,
                },
            },
            cx,
        );
        assert_eq!(
            store.delivery_messages_for(store.sole_thread().as_deref(), cx),
            vec![(accepted_key.clone(), "hello".into(), None, true)]
        );
        store.show_alone("another".into(), cx);
        // Adoption must survive navigation even when no history is retained.
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: "scripted".into(),
                },
                event: ServerEvent::SessionEvent(same_text_record(format!(
                    "local-user-{accepted_key}"
                ))),
            },
            cx,
        );
        store.threads.remove("scripted");
        store.show_alone("scripted".into(), cx);
        assert!(
            store
                .delivery_messages_for(store.sole_thread().as_deref(), cx)
                .is_empty()
        );
    });
    store.update(cx, |store, _| {
        store.send_turn_for(
            store.sole_thread().unwrap_or_default(),
            "hello".into(),
            Vec::new(),
        )
    });
    let early = next_command();
    let early_key = early.key.unwrap();
    store.update(cx, |store, cx| {
        store.show_alone("another".into(), cx);
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: "scripted".into(),
                },
                event: ServerEvent::SessionEvent(same_text_record(format!(
                    "local-user-{early_key}"
                ))),
            },
            cx,
        );
    });
    replies
        .send_blocking(
            tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                id: early.id,
                result: Ok(tcode_protocol::CommandResponse::Unit),
            })
            .unwrap(),
        )
        .unwrap();
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    store.update(cx, |store, cx| store.show_alone("scripted".into(), cx));
    assert!(
        store.read_with(cx, |store, cx| store
            .delivery_messages_for(store.sole_thread().as_deref(), cx)
            .is_empty()),
        "a replica received before its Ack must stay adopted"
    );
    store.update(cx, |store, _| {
        store.send_turn_for(
            store.sole_thread().unwrap_or_default(),
            "discard me".into(),
            Vec::new(),
        )
    });
    let request = next_command();
    replies
        .send_blocking(
            tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                id: request.id,
                result: Err(tcode_protocol::ProtocolError {
                    code: "unknown_session".into(),
                    message: "gone".into(),
                }),
            })
            .unwrap(),
        )
        .unwrap();
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    store.update(cx, |store, _| {
        store.discard_delivery(request.key.as_ref().unwrap())
    });
    assert!(link.failed_commands().is_empty());
    assert!(link.pending_commands().is_empty());
    link.close();
}

#[gpui::test]
fn archived_list_coalesces_revision_changes_and_reloads_a_stale_reply(cx: &mut TestAppContext) {
    let (to_host, requests) = async_channel::unbounded();
    let (replies, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
    let mut pump = std::pin::pin!(link.pump());
    let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
    let queries = || {
        std::iter::from_fn(|| requests.try_recv().ok())
            .filter_map(|line| {
                let message = tcode_protocol::decode_client_line(&line).unwrap();
                matches!(
                    message.payload,
                    tcode_protocol::ClientPayload::Query(tcode_protocol::Query::ArchivedSessions)
                )
                .then_some(message.id)
            })
            .collect::<Vec<_>>()
    };
    let revision_event = |revision| EventEnvelope {
        request_id: None,
        topic: Topic::Index,
        event: ServerEvent::IndexSummaryReplaced(tcode_protocol::IndexSummary {
            archived_revision: revision,
            ..Default::default()
        }),
    };
    store.update(cx, |store, cx| {
        store.receive(revision_event(10), cx);
        store.hold_archived_sessions(cx);
    });
    cx.run_until_parked();
    let first = queries();
    assert_eq!(first.len(), 1);
    store.update(cx, |store, cx| {
        store.receive(revision_event(11), cx);
        store.receive(revision_event(12), cx);
    });
    cx.run_until_parked();
    let response = |id, revision, title: &str| {
        let mut meta = SessionMeta::new(
            ProviderKind::Codex,
            std::path::PathBuf::from("/archive"),
            None,
        );
        meta.id = "archived".into();
        meta.title = title.into();
        meta.archived_at = Some(1);
        replies
            .try_send(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                    id,
                    result: Ok(tcode_protocol::QueryResponse::ArchivedSessions(
                        tcode_protocol::ArchivedSessions {
                            sessions: vec![meta],
                            worktree_shared: Default::default(),
                            revision,
                        },
                    )),
                })
                .unwrap(),
            )
            .unwrap();
    };
    response(first[0], 10, "Stale title");
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    cx.run_until_parked();
    let second = queries();
    assert_eq!(second.len(), 1);
    assert!(store.read_with(cx, |store, _| store.archived_loading()));
    response(second[0], 12, "Current title");
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    cx.run_until_parked();
    store.read_with(cx, |store, _| {
        let archived = store.archived_replica.as_ref().unwrap();
        assert_eq!(archived.revision, 12);
        assert_eq!(archived.sessions[0].title, "Current title");
    });
    store.update(cx, |store, cx| store.receive(revision_event(13), cx));
    cx.run_until_parked();
    let third = queries();
    assert_eq!(third.len(), 1, "an archived rename invalidates a held list");
    response(third[0], 13, "Renamed again");
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    cx.run_until_parked();
    store.update(cx, |store, cx| {
        let state = tcode_client::ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        };
        store.host.set_connection_state(state.clone());
        store.apply_connection_state(state);
        let state = tcode_client::ConnectionState::Syncing { path: None };
        store.host.set_connection_state(state.clone());
        store.apply_connection_state(state);
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::Index,
                event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    summary: tcode_protocol::IndexSummary {
                        archived_revision: 1,
                        ..Default::default()
                    },
                    sessions: Vec::new(),
                    projects: Vec::new(),
                }),
            },
            cx,
        );
    });
    cx.run_until_parked();
    let reconnect = queries();
    assert_eq!(reconnect.len(), 1);
    response(reconnect[0], 1, "New host baseline");
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    cx.run_until_parked();
    store.update(cx, |store, cx| {
        assert_eq!(
            store.archived_replica.as_ref().unwrap().sessions[0].title,
            "New host baseline"
        );
        // Settings and the Agents view hold the list independently.
        store.hold_archived_sessions(cx);
        store.release_archived_sessions();
        assert!(
            !store.archived_loading(),
            "one view closing keeps another's list"
        );
        store.release_archived_sessions();
        assert!(store.archived_loading());
    });
    link.close();
}

/// A load the host refuses is shown with the host's words, and the
/// summary repeating the same archived revision does not ask again; a new
/// revision does.
#[gpui::test]
fn archived_list_that_fails_says_why_and_waits_for_a_new_revision(cx: &mut TestAppContext) {
    let (to_host, requests) = async_channel::unbounded();
    let (replies, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
    let mut pump = std::pin::pin!(link.pump());
    let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
    let queries = || {
        std::iter::from_fn(|| requests.try_recv().ok())
            .filter_map(|line| {
                let message = tcode_protocol::decode_client_line(&line).unwrap();
                matches!(
                    message.payload,
                    tcode_protocol::ClientPayload::Query(tcode_protocol::Query::ArchivedSessions)
                )
                .then_some(message.id)
            })
            .collect::<Vec<_>>()
    };
    let revision_event = |revision| EventEnvelope {
        request_id: None,
        topic: Topic::Index,
        event: ServerEvent::IndexSummaryReplaced(tcode_protocol::IndexSummary {
            archived_revision: revision,
            ..Default::default()
        }),
    };
    store.update(cx, |store, cx| {
        store.receive(revision_event(10), cx);
        store.hold_archived_sessions(cx);
    });
    cx.run_until_parked();
    let asked = queries();
    assert_eq!(asked.len(), 1);
    replies
        .try_send(
            tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                id: asked[0],
                result: Err(tcode_protocol::ProtocolError {
                    code: "storage".into(),
                    message: "the archive index is unreadable".into(),
                }),
            })
            .unwrap(),
        )
        .unwrap();
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    cx.run_until_parked();
    store.read_with(cx, |store, _| {
        assert_eq!(
            store.archived_failure(),
            Some("the archive index is unreadable")
        );
        assert!(!store.archived_loading());
    });
    store.update(cx, |store, cx| store.receive(revision_event(10), cx));
    cx.run_until_parked();
    assert!(queries().is_empty(), "the same revision is not asked again");
    store.update(cx, |store, cx| store.receive(revision_event(11), cx));
    cx.run_until_parked();
    assert_eq!(queries().len(), 1, "a new revision is asked for");
    link.close();
}

/// The threads list never says "no threads" before the host's baseline.
/// A host reached over a link is waited for behind a skeleton, which
/// fades out over the baseline; the host in this process answers within
/// moments, so its list stays empty until then and shows at once, with
/// no skeleton drawn at launch.
#[gpui::test]
fn threads_wait_for_baseline_before_rendering_empty(cx: &mut TestAppContext) {
    use gpui::{px, size};
    cx.update(crate::test_client::init);
    for remote in [true, false] {
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                tcode_client::HostLink::new(to_host, from_host),
                if remote {
                    super::WorkspaceAttachment::Remote {
                        host_id: "test".into(),
                        host_name: "Test".into(),
                    }
                } else {
                    super::WorkspaceAttachment::Local
                },
                None,
                None,
                cx,
            )
        });
        seed_full_scope(&store, &incoming, Vec::new(), cx);
        let window_state =
            cx.new(|_| crate::window_state::WindowState::default().with_compact(true));
        let (_sidebar, cx) = cx.add_window_view(|_, cx| {
            crate::sidebar::SessionsSidebar::new(vec![store.clone()], window_state, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(
            cx.debug_bounds("baseline-loading").is_some(),
            remote,
            "a skeleton before Index arrives only for a host over a link"
        );
        assert!(cx.debug_bounds("threads-empty").is_none());
        store.update(cx, |store, cx| {
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::SettingsSnapshot(Settings::default()),
                },
                cx,
            );
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                        summary: Default::default(),
                        sessions: vec![],
                        projects: vec![],
                    }),
                },
                cx,
            );
            cx.emit(super::StoreChange {
                topic: super::TopicKind::Index,
            });
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(
            cx.debug_bounds("threads-empty").is_some(),
            "the baseline replaces what waited for it"
        );
    }
}

#[gpui::test]
fn deleted_thread_keeps_its_pending_and_rejected_send_visible(cx: &mut TestAppContext) {
    let root = scratch_root("tcode-deleted-pending");
    let disk = SessionStore::open_at(root.clone()).unwrap();
    disk.upsert_project(&project_at("p", &root)).unwrap();
    disk.upsert_meta(&thread(&root, "deleted", "p", None))
        .unwrap();
    let host = test_host(disk);
    let mux = tcode_traverse::mux::HostMux::new(host.attachments.clone(), host.from_host.clone());
    let (mut driver, mut elsewhere) = (attach(&mux), attach(&mux));
    let link = driver.link.clone();
    let workspace = driver.workspace(cx);
    workspace.update(cx, |store, cx| store.show_alone("deleted".into(), cx));
    driver.wait_until(cx, "selected thread", |cx| {
        selected_status(cx, &workspace, "deleted")
    });
    link.set_connection_state(tcode_client::ConnectionState::Reconnecting {
        attempt: 1,
        reason: None,
    });
    workspace.update(cx, |store, _| {
        store.send_turn_for(
            store.sole_thread().unwrap_or_default(),
            "keep my failed send".into(),
            Vec::new(),
        )
    });
    // Another device deletes the thread while this one is away.
    elsewhere.command(
        cx,
        Command::DeleteSession {
            session_id: "deleted".into(),
            remove_worktree: false,
        },
    );
    driver.wait_until(cx, "the thread's removal", |cx| {
        workspace.read_with(cx, |store, _| store.thread_meta("deleted").is_none())
    });
    assert_eq!(
        workspace.read_with(cx, |store, _| store.sole_thread()),
        Some("deleted".into())
    );
    link.set_connection_state(tcode_client::ConnectionState::Connected { path: None });
    driver.wait_until(cx, "rejected send", |cx| {
        workspace.read_with(cx, |store, cx| {
            store
                .delivery_messages_for(store.sole_thread().as_deref(), cx)
                .iter()
                .any(|(_, _, error, _)| error.is_some())
        })
    });
    workspace.read_with(cx, |store, cx| {
        assert_eq!(store.sole_thread().as_deref(), Some("deleted"));
        assert_eq!(
            store.delivery_messages_for(store.sole_thread().as_deref(), cx)[0].1,
            "keep my failed send"
        );
        assert!(!store.chat_loading_for(store.sole_thread().as_deref(), cx));
    });
    let key = link.failed_commands()[0].0.key.clone();
    workspace.update(cx, |store, _| store.discard_delivery(&key));
    assert!(link.pending_commands().is_empty() && link.failed_commands().is_empty());
    shutdown_attached(cx, &host, vec![driver, elsewhere]);
    let _ = std::fs::remove_dir_all(root);
}

#[gpui::test]
fn rapid_selection_is_immediate_idempotent_and_retires_old_loads(cx: &mut TestAppContext) {
    let (to_host, outgoing) = async_channel::unbounded();
    let (_incoming, from_host) = async_channel::unbounded();
    let host = tcode_client::HostLink::new(to_host, from_host);
    let workspace = cx.new(|cx| {
        WorkspaceStore::new_attached(host.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    seed_full_scope(&workspace, &_incoming, Vec::new(), cx);
    workspace.update(cx, |store, cx| {
        for index in 0..2 {
            let id = if index % 2 == 0 { "one" } else { "two" };
            store.show_alone(id.into(), cx);
            assert_eq!(store.sole_thread().as_deref(), Some(id));
            assert!(store.session_loading_for(&store.sole_thread().unwrap_or_default()));
            let messages = outgoing.len();
            store.show_alone(id.into(), cx);
            assert_eq!(outgoing.len(), messages, "second tap must send nothing");
            assert_eq!(
                host.subscriptions()
                    .iter()
                    .filter(|sub| matches!(sub.topic, Topic::SessionEvents { .. }))
                    .count(),
                1
            );
            assert_eq!(
                host.subscriptions()
                    .iter()
                    .filter(|sub| matches!(sub.topic, Topic::SessionStatus { .. }))
                    .count(),
                1
            );
        }
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: "one".into(),
                },
                event: ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 0,
                    records: vec![],
                },
            },
            cx,
        );
        assert!(
            store.session_loading_for(&store.sole_thread().unwrap_or_default()),
            "retired load must not replace the skeleton"
        );
        assert!(store.thread_timeline(Some("two"), cx).is_none());
    });
}

#[gpui::test]
fn rapid_thread_revisit_keeps_cached_history_when_old_replies_arrive(cx: &mut TestAppContext) {
    for old_first in [false, true] {
        for empty in [false, true] {
            let (to_host, _outgoing) = async_channel::unbounded();
            let (incoming, from_host) = async_channel::unbounded();
            let host = tcode_client::HostLink::new(to_host, from_host);
            let workspace = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    host.clone(),
                    WorkspaceAttachment::Local,
                    None,
                    None,
                    cx,
                )
            });
            seed_full_scope(&workspace, &incoming, Vec::new(), cx);
            workspace.update(cx, |store, cx| {
                    let topic = Topic::SessionEvents { session_id: "a".into() };
                    store.show_alone("a".into(), cx);
                    let record = |text: &str| StoredEvent::from(AgentEvent::Delta {
                        item_id: "answer".into(), kind: agent::DeltaKind::AssistantText, text: text.into(),
                    });
                    let baseline_id = host.subscription_request_id(&topic).unwrap();
                    store.receive(EventEnvelope {
                        request_id: Some(baseline_id), topic: topic.clone(),
                        event: ServerEvent::SessionSnapshot { from: 10, end: 12, records: vec![record("cached")], total: 12, total_turns: 1, truncated: false },
                    }, cx);
                    host.subscribe(Subscription { topic: topic.clone(), after: Some(12), known_index: None }).unwrap();
                    let old_id = host.subscription_request_id(&topic).unwrap();
                    store.show_alone("b".into(), cx);
                    store.show_alone("a".into(), cx);
                    let current_id = host.subscription_request_id(&topic).unwrap();
                    assert_ne!(old_id, current_id);
                    let reply = |id| EventEnvelope {
                        request_id: Some(id), topic: topic.clone(),
                        event: ServerEvent::SessionSnapshot {
                            from: 12, end: if empty { 12 } else { 14 },
                            records: if empty { vec![] } else { vec![record(" continued")] },
                            total: if empty { 12 } else { 14 }, total_turns: 1, truncated: false,
                        },
                    };
                    for id in if old_first { [old_id, current_id] } else { [current_id, old_id] } {
                        store.receive(reply(id), cx);
                    }
                    store.receive(reply(current_id), cx);
                    let timeline = &store.thread_timeline(Some("a"), cx).unwrap();
                    assert_eq!(timeline.entries.len(), 1);
                    assert!(matches!(&timeline.entries[0].content, EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == if empty { "cached" } else { "cached continued" }));
                    let end = if empty { 12 } else { 14 };
                    host.subscribe(Subscription { topic: topic.clone(), after: None, known_index: None }).unwrap();
                    let replacement_id = host.subscription_request_id(&topic).unwrap();
                    let replacement = EventEnvelope { request_id: Some(replacement_id), topic: topic.clone(), event: ServerEvent::SessionSnapshot { from: end, end: end + 2, records: vec![record("replacement")], total: end + 2, total_turns: 1, truncated: false } };
                    store.receive(replacement.clone(), cx);
                    store.receive(reply(current_id), cx);
                    store.receive(replacement, cx);
                    let timeline = &store.thread_timeline(Some("a"), cx).unwrap();
                    assert_eq!(timeline.entries.len(), 1);
                    assert!(matches!(&timeline.entries[0].content, EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "replacement"));
                });
        }
    }
}

#[gpui::test]
fn paged_history_keeps_event_and_turn_cursors_absolute(cx: &mut TestAppContext) {
    let (to_host, outgoing) = async_channel::unbounded();
    let (_incoming, from_host) = async_channel::unbounded();
    let host = tcode_client::HostLink::new(to_host, from_host);
    let workspace = cx.new(|cx| {
        WorkspaceStore::new_attached(host.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    seed_full_scope(&workspace, &_incoming, Vec::new(), cx);
    workspace.update(cx, |store, cx| {
        store.show_alone("large".into(), cx);
        let records = (450..500)
            .flat_map(|index| {
                [
                    AgentEvent::TurnStarted {
                        turn_id: index.to_string(),
                    }
                    .into(),
                    AgentEvent::ItemCompleted(ThreadItem {
                        id: format!("user-{index}"),
                        parent_item_id: None,
                        content: ItemContent::UserMessage {
                            text: format!("Message {index}"),
                            attachments: vec![],
                        },
                    })
                    .into(),
                    AgentEvent::ItemCompleted(ThreadItem {
                        id: format!("assistant-{index}"),
                        parent_item_id: None,
                        content: ItemContent::AssistantMessage {
                            text: format!("Response {index}"),
                            summarized: false,
                        },
                    })
                    .into(),
                    AgentEvent::TurnCompleted {
                        turn_id: index.to_string(),
                        status: TurnStatus::Completed,
                        usage: None,
                    }
                    .into(),
                ]
            })
            .collect();
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: "large".into(),
                },
                event: ServerEvent::SessionSnapshot {
                    from: 1800,
                    end: 2000,
                    records,
                    total: 2000,
                    total_turns: 500,
                    truncated: false,
                },
            },
            cx,
        );
        assert_eq!(
            host.subscriptions()
                .iter()
                .find(|sub| matches!(sub.topic, Topic::SessionEvents { .. }))
                .unwrap()
                .after,
            Some(2000)
        );
        while outgoing.try_recv().is_ok() {}
        store.rewind_turn_for(
            store.sole_thread().unwrap_or_default(),
            2,
            None,
            None,
            agent::RewindMode::Conversation,
        );
        let command = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert!(matches!(
            command.payload,
            tcode_protocol::ClientPayload::Command(Command::RewindTurn { turn: 452, .. })
        ));
    });
}

/// The host merges records, so the cursors a client resumes from are the
/// window it was sent, not the records it holds; visit times and index
/// facts arrive as changes rather than replacements.
#[gpui::test]
fn merged_windows_visits_and_summaries_apply_as_changes(cx: &mut TestAppContext) {
    use std::collections::{HashMap, HashSet};
    use tcode_core::session::StoredEvent;
    use tcode_protocol::IndexSummary;
    let (to_host, _outgoing) = async_channel::unbounded();
    let (_incoming, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let workspace =
        cx.new(|cx| WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx));
    crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
    let topic = Topic::SessionEvents {
        session_id: "merged".into(),
    };
    let tool = agent::AgentEvent::ItemCompleted(agent::ThreadItem {
        id: "shot".into(),
        parent_item_id: None,
        content: ItemContent::ToolCall {
            image_reads: Vec::new(),
            name: "screenshot".into(),
            input: serde_json::json!({}),
            output: Some("preview".into()),
            status: agent::ItemStatus::Completed,
            family: None,
        },
    });
    workspace.update(cx, |store, cx| {
        store.show_alone("merged".into(), cx);
        store.settings_replica.last_visited = HashMap::from([("old".into(), 1)]);
        let event = |event| EventEnvelope {
            request_id: None,
            topic: topic.clone(),
            event,
        };
        store.receive(
            event(ServerEvent::SessionSnapshot {
                from: 10,
                end: 20,
                records: vec![StoredEvent {
                    origin: None,
                    author: None,
                    ts: Some(1),
                    event: tool.clone(),
                    elided: Some(700_000),
                }],
                total: 20,
                total_turns: 1,
                truncated: false,
            }),
            cx,
        );
        assert_eq!(
            store.with_timeline_for(
                store.sole_thread().as_deref(),
                |timeline| timeline.elided_outputs.get("shot").copied(),
                cx,
            ),
            Some(Some(700_000))
        );
        store.receive(
            event(ServerEvent::SessionEvent(
                agent::AgentEvent::TurnStarted {
                    turn_id: "next".into(),
                }
                .into(),
            )),
            cx,
        );
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::Settings,
                event: ServerEvent::LastVisitedChanged(HashMap::from([("new".into(), 2)])),
            },
            cx,
        );
        assert_eq!(
            store.settings_replica.last_visited,
            HashMap::from([("old".into(), 1), ("new".into(), 2)])
        );
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::Index,
                event: ServerEvent::IndexSummaryReplaced(IndexSummary {
                    title_generating: HashSet::from(["named".into()]),
                    ..IndexSummary::default()
                }),
            },
            cx,
        );
        assert!(store.title_generating("named"));
    });
}

/// A device with full access to a remote machine is the owner's own: the
/// owner's unattributed messages carry no byline there, while a space
/// member's message is still attributed.
#[gpui::test]
fn a_full_remote_device_attributes_only_space_members(cx: &mut TestAppContext) {
    use tcode_core::session::{Author, StoredEvent};
    let (to_host, _outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let workspace = cx.new(|cx| {
        WorkspaceStore::new_attached(
            link,
            WorkspaceAttachment::Remote {
                host_id: "machine".into(),
                host_name: "Machine".into(),
            },
            None,
            None,
            cx,
        )
    });
    crate::store::tests::seed_full_scope(&workspace, &incoming, Vec::new(), cx);
    let message = |id: &str, author: Option<Author>| StoredEvent {
        origin: None,
        author,
        ts: Some(1),
        event: agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: id.into(),
                attachments: Vec::new(),
            },
        }),
        elided: None,
    };
    let snapshot = |records: Vec<StoredEvent>| EventEnvelope {
        request_id: None,
        topic: Topic::SessionEvents {
            session_id: "shared".into(),
        },
        event: ServerEvent::SessionSnapshot {
            from: 0,
            end: records.len() as u64,
            total: records.len() as u64,
            records,
            total_turns: 1,
            truncated: false,
        },
    };
    workspace.update(cx, |store, cx| {
        store.show_alone("shared".into(), cx);
        store.receive(snapshot(vec![message("owner", None)]), cx);
        assert_eq!(
            store.message_byline_for(store.sole_thread().as_deref(), "owner", cx),
            None
        );

        let member = Author {
            device_id: "member-device".into(),
            name: "Alex's laptop".into(),
        };
        store.receive(snapshot(vec![message("member", Some(member))]), cx);
        assert_eq!(
            store
                .message_byline_for(store.sole_thread().as_deref(), "member", cx)
                .as_deref(),
            Some("Alex's laptop")
        );
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: "shared".into(),
                },
                event: ServerEvent::SessionEvent(message("owner", None)),
            },
            cx,
        );
        assert_eq!(
            store
                .message_byline_for(store.sole_thread().as_deref(), "owner", cx)
                .as_deref(),
            Some(crate::tr!("member.owner").as_ref())
        );
        assert_eq!(
            store
                .message_byline_for(store.sole_thread().as_deref(), "member", cx)
                .as_deref(),
            Some("Alex's laptop")
        );

        // An orchestrator's message to its child is no member's, the owner's included.
        let mut lead = message("lead", None);
        lead.origin = Some(tcode_core::session::MessageOrigin::Agent);
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: "shared".into(),
                },
                event: ServerEvent::SessionEvent(lead),
            },
            cx,
        );
        assert_eq!(
            store
                .message_byline_for(store.sole_thread().as_deref(), "lead", cx)
                .as_deref(),
            Some(crate::tr!("chat.from_orchestrator").as_ref())
        );
    });
}

/// A thread is read only once its conversation has loaded and is on
/// screen, and stays read through updates that land while it is shown. A
/// view left before its conversation arrived reports nothing.
#[gpui::test]
fn a_thread_is_reported_read_once_its_conversation_loads(cx: &mut TestAppContext) {
    let (to_host, outgoing) = async_channel::unbounded();
    let (_incoming, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let workspace =
        cx.new(|cx| WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx));
    crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
    let reads = || {
        std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(
                |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                    tcode_protocol::ClientPayload::Command(Command::MarkSessionRead {
                        session_id,
                        through,
                    }) => Some((session_id, through)),
                    _ => None,
                },
            )
            .collect::<Vec<_>>()
    };
    let upsert = |id: &str, updated_at| {
        let mut meta =
            SessionMeta::new(ProviderKind::Codex, std::path::PathBuf::from("/tmp"), None);
        meta.id = id.into();
        meta.updated_at = updated_at;
        EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexUpsertSession(Box::new(meta)),
        }
    };
    workspace.update(cx, |store, cx| {
        store.receive(upsert("left", 100), cx);
        store.receive(upsert("shown", 100), cx);
        store.settings_replica.last_visited =
            std::collections::HashMap::from([("left".into(), 50), ("shown".into(), 50)]);

        store.set_conversation_on_screen(true, cx);
        store.show_alone("left".into(), cx);
        store.leave_threads(cx);
        store.receive(session_snapshot("left", 0, vec![reply(1)]), cx);
        assert_eq!(reads(), [], "left before the conversation loaded");

        store.show_alone("shown".into(), cx);
        assert_eq!(reads(), [], "nothing has loaded yet");
        // A compact window went back to the thread list, which keeps the
        // thread selected, before the conversation arrived.
        store.set_conversation_on_screen(false, cx);
        store.receive(session_snapshot("shown", 0, vec![reply(1)]), cx);
        assert_eq!(reads(), [], "loaded behind the thread list");
        store.set_conversation_on_screen(true, cx);
        assert_eq!(reads(), [("shown".to_string(), 100)]);
        store.receive(upsert("left", 110), cx);
        assert_eq!(reads(), [], "unchanged for the thread on screen");

        store.receive(upsert("shown", 120), cx);
        assert_eq!(reads(), [("shown".to_string(), 120)]);
    });
}

/// The status a host gives a draft it starts, read as a client reads
/// it: the draft started with `StartDraft`, its status topic's snapshot.
pub(crate) fn draft_status() -> tcode_protocol::SessionStatus {
    let root = scratch_root("tcode-draft-status-test");
    let host = test_host(SessionStore::open_at(root.clone()).unwrap());
    let link = host.link();
    let events = link.events();
    let tcode_protocol::CommandResponse::SessionId(Some(session_id)) = link
        .command_blocking(Command::StartDraft {
            project_id: "draft".into(),
            cwd: std::env::temp_dir(),
        })
        .expect("start a draft")
    else {
        panic!("the host starts a draft");
    };
    link.subscribe(tcode_protocol::Subscription {
        known_index: None,
        topic: Topic::SessionStatus {
            session_id: session_id.clone(),
        },
        after: None,
    })
    .expect("subscribe to the draft's status");
    let status = loop {
        let envelope = events.recv_blocking().expect("the host's events");
        if let ServerEvent::SessionStatusReplaced(status) = envelope.event
            && status.session_id == session_id
        {
            break *status;
        }
    };
    host.shutdown_blocking().expect("stop the fixture host");
    let _ = std::fs::remove_dir_all(root);
    status
}

fn with_running_turn(
    status: &tcode_protocol::SessionStatus,
    running: Option<tcode_core::session::RunningTurn>,
    question: Option<tcode_core::session::PendingUserInput>,
) -> tcode_protocol::SessionStatus {
    let mut status = status.clone();
    status.activity.turn_running = running.is_some();
    status.activity.working = running.is_some();
    status.running_turn = running;
    status.pending_user_input = question;
    status
}

fn question() -> tcode_core::session::PendingUserInput {
    tcode_core::session::PendingUserInput {
        request_id: "ask".into(),
        questions: vec![agent::UserInputQuestion {
            id: "which".into(),
            header: "Next".into(),
            question: "Which way?".into(),
            options: Vec::new(),
            multi_select: false,
            prefill: None,
            optional: false,
        }],
        delivery: agent::UserInputDelivery::Blocking,
    }
}

fn recorded(ts: u64, event: AgentEvent) -> StoredEvent {
    StoredEvent {
        origin: None,
        author: None,
        ts: Some(ts),
        ..event.into()
    }
}

fn reply(ts: u64) -> StoredEvent {
    recorded(
        ts,
        AgentEvent::ItemCompleted(ThreadItem {
            id: format!("reply-{ts}"),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "working".into(),
                summarized: false,
            },
        }),
    )
}

fn session_snapshot(session_id: &str, from: u64, records: Vec<StoredEvent>) -> EventEnvelope {
    let end = from + records.len() as u64;
    EventEnvelope {
        request_id: None,
        topic: Topic::SessionEvents {
            session_id: session_id.into(),
        },
        event: ServerEvent::SessionSnapshot {
            from,
            end,
            records,
            total: end,
            total_turns: 1,
            truncated: from > 0,
        },
    }
}

fn live_turn(workspace: &gpui::Entity<WorkspaceStore>, cx: &TestAppContext) -> Option<u64> {
    workspace.read_with(cx, |store, cx| {
        store
            .with_timeline_for(
                store.sole_thread().as_deref(),
                |timeline| {
                    timeline
                        .turns
                        .last()
                        .filter(|turn| turn.running)
                        .and_then(|turn| turn.start_ts)
                },
                cx,
            )
            .flatten()
    })
}

#[gpui::test]
fn the_status_settles_the_running_turn_whichever_topic_arrives_first(cx: &mut TestAppContext) {
    let status = draft_status();
    let id = status.session_id.clone();
    let status_event = |running: Option<u64>, question| EventEnvelope {
        request_id: None,
        topic: Topic::SessionStatus {
            session_id: id.clone(),
        },
        event: ServerEvent::SessionStatusReplaced(Box::new(with_running_turn(
            &status,
            running.map(|turn| tcode_core::session::RunningTurn {
                turn,
                started_at: Some(1_000),
            }),
            question,
        ))),
    };
    let snapshot = session_snapshot(
        &id,
        0,
        vec![
            recorded(
                1_000,
                AgentEvent::TurnStarted {
                    turn_id: "turn".into(),
                },
            ),
            reply(2_000),
            recorded(
                3_000,
                AgentEvent::UserInputRequested {
                    request_id: "ask".into(),
                    questions: question().questions,
                    delivery: agent::UserInputDelivery::Blocking,
                },
            ),
        ],
    );
    for status_first in [false, true] {
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
        let open_question = |cx: &TestAppContext| {
            workspace.read_with(cx, |store, cx| {
                store
                    .composer_state_for(store.sole_thread().as_deref(), cx)
                    .pending_user_input
                    .map(|pending| pending.request_id)
            })
        };
        workspace.update(cx, |store, cx| {
            // The status cached from the last visit, before this turn began.
            store.show_alone(id.clone(), cx);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: id.clone(),
                    },
                    event: ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
                },
                cx,
            );
            store.leave_threads(cx);
            store.show_alone(id.clone(), cx);
            if status_first {
                store.receive(status_event(Some(0), Some(question())), cx);
                store.receive(snapshot.clone(), cx);
            } else {
                store.receive(snapshot.clone(), cx);
                assert_eq!(
                    store.with_timeline_for(store.sole_thread().as_deref(), |t| t.turn_running, cx),
                    Some(false)
                );
                store.receive(status_event(Some(0), Some(question())), cx);
            }
        });
        assert_eq!(live_turn(&workspace, cx), Some(1_000));
        assert_eq!(open_question(cx).as_deref(), Some("ask"));

        // The next turn runs before its opening record arrives.
        workspace.update(cx, |store, cx| {
            store.receive(status_event(Some(1), None), cx)
        });
        assert_eq!(live_turn(&workspace, cx), None);
        assert_eq!(
            workspace.read_with(cx, |store, cx| store.with_timeline_for(
                store.sole_thread().as_deref(),
                |timeline| timeline.turn_running,
                cx,
            )),
            Some(true)
        );

        workspace.update(cx, |store, cx| store.receive(status_event(None, None), cx));
        assert_eq!(live_turn(&workspace, cx), None);
        assert_eq!(open_question(cx), None);
    }
}

#[gpui::test]
fn open_threads_each_read_their_own_replica(cx: &mut TestAppContext) {
    let a = draft_status();
    let mut b = with_running_turn(&a, None, Some(question()));
    b.session_id = "thread-b".into();
    let status = |status: &tcode_protocol::SessionStatus| EventEnvelope {
        request_id: None,
        topic: Topic::SessionStatus {
            session_id: status.session_id.clone(),
        },
        event: ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
    };
    let (to_host, _outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let workspace =
        cx.new(|cx| WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx));
    crate::store::tests::seed_full_scope(&workspace, &incoming, deferred, cx);
    workspace.update(cx, |store, cx| {
        store.show_alone(a.session_id.clone(), cx);
        store.open_thread(&b.session_id, cx);
        store.receive(status(&a), cx);
        store.receive(session_snapshot(&a.session_id, 0, vec![reply(1)]), cx);
        store.receive(status(&b), cx);
        store.receive(session_snapshot(&b.session_id, 0, vec![reply(2)]), cx);
    });
    let shown = |store: &WorkspaceStore, id: &str, cx: &App| {
        (
            store
                .composer_state_for(Some(id), cx)
                .pending_user_input
                .map(|pending| pending.request_id),
            store.with_timeline_for(
                Some(id),
                |timeline| {
                    timeline
                        .entries
                        .iter()
                        .map(|entry| entry.id.clone())
                        .collect::<Vec<_>>()
                },
                cx,
            ),
        )
    };
    let a_shows = (None, Some(vec!["reply-1".to_string()]));
    let b_shows = (Some("ask".to_string()), Some(vec!["reply-2".to_string()]));
    workspace.read_with(cx, |store, cx| {
        assert_eq!(shown(store, &a.session_id, cx), a_shows);
        assert_eq!(shown(store, &b.session_id, cx), b_shows);
    });

    // Each keeps receiving its events.
    workspace.update(cx, |store, cx| {
        store.receive(status(&with_running_turn(&a, None, Some(question()))), cx);
    });
    workspace.read_with(cx, |store, cx| {
        assert_eq!(
            shown(store, &a.session_id, cx),
            (b_shows.0.clone(), a_shows.1)
        );
        assert_eq!(shown(store, &b.session_id, cx), b_shows);
    });
}

#[gpui::test]
fn a_window_cut_inside_the_running_turn_is_timed_from_the_host(cx: &mut TestAppContext) {
    let status = with_running_turn(
        &draft_status(),
        Some(tcode_core::session::RunningTurn {
            turn: 0,
            started_at: Some(1_000),
        }),
        None,
    );
    let id = status.session_id.clone();
    let (to_host, outgoing) = async_channel::unbounded();
    let (_incoming, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let workspace =
        cx.new(|cx| WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx));
    crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
    workspace.update(cx, |store, cx| {
        store.show_alone(id.clone(), cx);
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: id.clone(),
                },
                event: ServerEvent::SessionStatusReplaced(Box::new(status)),
            },
            cx,
        );
        store.receive(session_snapshot(&id, 3, vec![reply(4_000)]), cx);
    });
    cx.run_until_parked();
    assert_eq!(live_turn(&workspace, cx), Some(1_000));
    assert!(
        !std::iter::from_fn(|| outgoing.try_recv().ok())
            .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
            .any(|request| matches!(
                request.payload,
                tcode_protocol::ClientPayload::Query(
                    tcode_protocol::Query::SessionHistoryPage { .. }
                )
            )),
        "the turn's start comes with the status, not from earlier pages"
    );
}

fn held_window<'a>(store: &'a WorkspaceStore, session_id: &str) -> Option<&'a HeldHistory> {
    store.threads.get(session_id)?.history.as_ref()
}

fn held_history<'a>(store: &'a WorkspaceStore, session_id: &str) -> &'a HeldHistory {
    held_window(store, session_id).expect("a held window")
}

fn test_host(store: SessionStore) -> SpawnedHost {
    spawn_host(store, HostServices::default()).expect("spawn test host")
}

/// A test host that starts providers with `launcher`.
fn launching_host(
    store: SessionStore,
    launcher: tcode_runtime::app::ProviderLauncher,
) -> SpawnedHost {
    spawn_host(
        store,
        HostServices {
            provider_launcher: launcher,
            ..HostServices::default()
        },
    )
    .expect("spawn test host")
}

/// What a test with no shell does in place of its one pane.
impl WorkspaceStore {
    /// The thread the pane shows, the one open.
    pub(crate) fn sole_thread(&self) -> Option<String> {
        let open = self.open_threads();
        assert!(open.len() < 2, "more than one thread is open: {open:?}");
        open.into_iter().next()
    }

    pub(crate) fn sole_status(&self, cx: &App) -> Option<&tcode_protocol::SessionStatus> {
        self.session_status_for(self.sole_thread().as_deref(), cx)
    }

    /// The pane shows `session_id` in place of what it showed.
    pub(crate) fn show_alone(&mut self, session_id: String, cx: &mut gpui::Context<Self>) {
        self.creation_request_generation = self.creation_request_generation.wrapping_add(1);
        self.draft_fallback_pending = false;
        self.remember_project_of(&session_id);
        for shown in self.open_threads() {
            if shown != session_id {
                self.close_thread(&shown, cx);
            }
        }
        if !self.is_thread_open(&session_id) {
            self.selection_moved_on();
            self.open_thread(&session_id, cx);
        }
        cx.notify();
    }

    /// The pane shows nothing.
    pub(crate) fn leave_threads(&mut self, cx: &mut gpui::Context<Self>) {
        self.selection_moved_on();
        self.draft_fallback_pending = false;
        for shown in self.open_threads() {
            self.close_thread(&shown, cx);
        }
        cx.notify();
    }
}

/// What the shell does with a thread to show, for a test without one.
pub(crate) fn place_shown_threads(store: &gpui::Entity<WorkspaceStore>, cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.subscribe(store, |store, shown: &super::ShowThread, cx| {
            store.update(cx, |store, cx| {
                store.show_alone(shown.session_id.clone(), cx)
            });
        })
        .detach();
    });
}

/// Wakes the test's thread from [`std::thread::park`]: a host answers from
/// threads of its own, and a future polled with this goes on once it has.
/// GPUI's test scheduler unparks the same thread when another thread wakes
/// one of its tasks.
struct Unpark(std::thread::Thread);

impl std::task::Wake for Unpark {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.unpark();
    }
}

/// A client of a real host whose replies the test's thread decodes, so
/// what they wake runs on GPUI's test scheduler. Between turns it waits
/// for the host as the app does: until the host sends something.
pub(crate) struct LocalHostDriver {
    pub(crate) link: tcode_client::HostLink,
    /// Gone once the host has closed its side of the link.
    pump: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>>,
    waker: std::task::Waker,
}

impl LocalHostDriver {
    pub(crate) fn new(host: &SpawnedHost) -> Self {
        let link = tcode_client::HostLink::new(host.to_host.clone(), host.from_host.clone());
        Self::from_link(link)
    }

    pub(crate) fn from_link(link: tcode_client::HostLink) -> Self {
        let pump_link = link.clone();
        Self {
            link,
            pump: Some(Box::pin(async move { pump_link.pump().await })),
            waker: std::sync::Arc::new(Unpark(std::thread::current())).into(),
        }
    }

    fn workspace(&mut self, cx: &mut TestAppContext) -> gpui::Entity<WorkspaceStore> {
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                self.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        place_shown_threads(&workspace, cx);
        self.wait_until(cx, "workspace baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        workspace
    }

    pub(crate) fn await_with_events<T>(
        &mut self,
        cx: &mut TestAppContext,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        let mut future = std::pin::pin!(future);
        let waker = self.waker.clone();
        let mut answer = None;
        self.wait_until(cx, "host reply", |_| {
            let mut task_cx = std::task::Context::from_waker(&waker);
            if let std::task::Poll::Ready(value) =
                std::future::Future::poll(future.as_mut(), &mut task_cx)
            {
                answer = Some(value);
                true
            } else {
                false
            }
        });
        answer.expect("reply observed")
    }

    /// What `receiver` hears next, the host turned until it has: a
    /// scripted provider hears from the host's threads, not the link.
    pub(crate) fn recv<T>(
        &mut self,
        cx: &mut TestAppContext,
        receiver: &async_channel::Receiver<T>,
    ) -> T {
        self.await_with_events(cx, receiver.recv())
            .expect("the sender is open")
    }

    /// Decodes what the host has sent, hands it to `store`, and runs what
    /// that woke.
    pub(crate) fn step(&mut self, cx: &mut TestAppContext) {
        if let Some(pump) = &mut self.pump {
            let mut task_cx = std::task::Context::from_waker(&self.waker);
            if pump.as_mut().poll(&mut task_cx).is_ready() {
                self.pump = None;
            }
        }
        cx.run_until_parked();
        cx.run_until_parked();
    }

    /// Waits until the host sends something or another thread wakes a
    /// task of the test's. The test fails if the host has closed the link
    /// while it still waits for `what`.
    pub(crate) fn wait_for_host(&self, what: &str) {
        assert!(
            self.pump.is_some(),
            "the host closed the link while the test waited for {what}"
        );
        std::thread::park();
    }

    pub(crate) fn wait_until(
        &mut self,
        cx: &mut TestAppContext,
        what: &str,
        mut ready: impl FnMut(&TestAppContext) -> bool,
    ) {
        loop {
            self.step(cx);
            if ready(cx) {
                return;
            }
            self.wait_for_host(what);
        }
    }

    /// [`Self::wait_until`] for a window, drawn at rest after each turn.
    pub(crate) fn settle_until(
        &mut self,
        cx: &mut gpui::VisualTestContext,
        what: &str,
        mut ready: impl FnMut(&mut gpui::VisualTestContext) -> bool,
    ) {
        loop {
            self.step(cx);
            crate::test_frames::draw_settled(cx);
            if ready(cx) {
                return;
            }
            self.wait_for_host(what);
        }
    }

    pub(crate) fn command(&mut self, cx: &mut TestAppContext, command: Command) {
        let link = self.link.clone();
        self.await_with_events(cx, link.command(command))
            .expect("typed host command");
    }

    /// Everything sent so far has reached the host, and what the host
    /// sent for it has reached the client: the host answers in order,
    /// and sends what an input changed before it reads the next.
    pub(crate) fn round_trip(&mut self, cx: &mut TestAppContext) {
        let link = self.link.clone();
        self.await_with_events(cx, link.query(tcode_protocol::Query::Ping))
            .expect("the host answers a ping");
        self.step(cx);
    }

    pub(crate) fn shutdown(&mut self, cx: &mut TestAppContext, host: &SpawnedHost) {
        let link = self.link.clone();
        self.await_with_events(cx, link.shutdown())
            .expect("stop host");
        host.stopped.recv_blocking().expect("host stopped");
    }
}

/// A client of `mux`'s host, attached as another device's app is.
pub(crate) fn attach(mux: &tcode_traverse::mux::HostMux) -> LocalHostDriver {
    let connection = mux.attach(
        tcode_protocol::Principal::Full,
        tcode_client::attachment::AttachmentPolicy::LOCAL,
    );
    LocalHostDriver::from_link(tcode_client::HostLink::new(
        connection.to_host,
        connection.from_host,
    ))
}

/// Stop a host whose clients attached through a mux: the first flushes
/// it, every client lets go, and the host's own channels close.
pub(crate) fn shutdown_attached(
    cx: &mut TestAppContext,
    host: &SpawnedHost,
    drivers: Vec<LocalHostDriver>,
) {
    let mut drivers = drivers;
    let link = drivers[0].link.clone();
    drivers[0]
        .await_with_events(cx, link.command(Command::ShutdownAllAndFlush))
        .expect("flush the host");
    for driver in &drivers {
        driver.link.close();
    }
    drop(drivers);
    host.to_host.close();
    host.from_host.close();
    host.stopped.recv_blocking().expect("host stopped");
}

#[gpui::test]
fn reconnect_restores_workspace_readiness_once_the_link_answers(cx: &mut TestAppContext) {
    use tcode_client::{ConnectionFailure, ConnectionState};
    let root = scratch_root("delayed-reconnect");
    let disk = SessionStore::open_at(root.clone()).unwrap();
    disk.upsert_project(&project_at("p", &root)).unwrap();
    disk.upsert_meta(&thread(&root, "one", "p", None)).unwrap();
    let host = test_host(disk);
    let mux = tcode_traverse::mux::HostMux::new(host.attachments.clone(), host.from_host.clone());
    let connection = mux.attach(
        tcode_protocol::Principal::Full,
        tcode_client::attachment::AttachmentPolicy::network(),
    );
    let (incoming, from_host) = async_channel::unbounded();
    let (outgoing, requests) = tcode_client::outgoing::channel();
    let link = tcode_client::HostLink::new(outgoing, from_host);
    let state_link = link.clone();
    let replies = async move {
        while let Ok(line) = connection.from_host.recv().await {
            if matches!(
                state_link.connection_state(),
                ConnectionState::Syncing { .. }
            ) {
                state_link.set_connection_state(ConnectionState::Connected { path: None });
            }
            if incoming.send(line).await.is_err() {
                break;
            }
        }
    };
    let mut driver = LocalHostDriver::from_link(link);
    let pump = driver.pump.take().unwrap();
    // A busy transport may not read until the whole recovery flight is queued.
    let forwards = async move {
        while let Ok(line) = requests.recv().await {
            if tcode_client::outgoing::subscription_key(&line).is_none() {
                requests.sent(&line);
            }
            if connection.to_host.send(line).await.is_err() {
                break;
            }
        }
    };
    driver.pump = Some(Box::pin(smol::future::race(
        pump,
        smol::future::race(forwards, replies),
    )));
    let workspace = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            WorkspaceAttachment::Remote {
                host_id: "test".into(),
                host_name: "test".into(),
            },
            None,
            None,
            cx,
        )
    });
    driver.wait_until(cx, "initial workspace baseline", |cx| {
        workspace.read_with(cx, |store, _| store.baseline_ready())
    });
    workspace.update(cx, |store, cx| store.show_alone("one".into(), cx));
    driver.wait_until(cx, "initial selected thread baseline", |cx| {
        workspace.read_with(cx, |store, _| store.baseline_ready())
    });
    driver
        .link
        .set_connection_state(ConnectionState::Reconnecting {
            attempt: 2,
            reason: Some(ConnectionFailure::Timeout),
        });
    cx.run_until_parked();
    assert!(!workspace.read_with(cx, |store, _| store.baseline_ready()));
    driver
        .link
        .set_connection_state(ConnectionState::Syncing { path: None });
    driver.wait_until(cx, "the reconnect baseline", |cx| {
        workspace.read_with(cx, |store, _| store.baseline_ready())
    });
    let link = driver.link.clone();
    driver
        .await_with_events(cx, link.command(Command::ShutdownAllAndFlush))
        .unwrap();
    driver.link.close();
    drop(driver);
    // A remote client closes its attachment; the local owner closes the host.
    host.to_host.close();
    host.from_host.close();
    host.stopped.recv_blocking().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

/// Two clients of one host show the same thread's preview: the first to
/// show it renders it, the other says where it is, and Show here moves
/// the page across, the first letting its page go.
#[cfg(preview_backend)]
#[gpui::test]
fn show_here_moves_a_threads_preview_between_clients(
    desk_cx: &mut TestAppContext,
    laptop_cx: &mut TestAppContext,
) {
    use crate::store::{PreviewPlace, PreviewReleased};
    let root = scratch_root("preview-handover");
    let disk = SessionStore::open_at(root.clone()).unwrap();
    disk.upsert_project(&project_at("p", &root)).unwrap();
    disk.upsert_meta(&thread(&root, "one", "p", None)).unwrap();
    let host = test_host(disk);
    let mux = tcode_traverse::mux::HostMux::new(host.attachments.clone(), host.from_host.clone());
    let client = |name: &str, cx: &mut TestAppContext| {
        let connection = mux.attach(
            tcode_protocol::Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let mut driver = LocalHostDriver::from_link(tcode_client::HostLink::new(
            connection.to_host,
            connection.from_host,
        ));
        cx.update(|cx| crate::device::DevicePreferences::set_device_name(Some(name.into()), cx));
        let store = driver.workspace(cx);
        store.update(cx, |store, cx| store.show_alone("one".into(), cx));
        (driver, store)
    };
    // Two devices, each its own app with its own name.
    let (mut desk_driver, desk) = client("Desk", desk_cx);
    let (mut laptop_driver, laptop) = client("Laptop", laptop_cx);
    let released = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let _released = desk_cx.update(|cx| {
        let released = released.clone();
        cx.subscribe(&desk, move |_, event: &PreviewReleased, _| {
            released.borrow_mut().push(event.session_id.clone())
        })
    });
    let place = |store: &gpui::Entity<WorkspaceStore>, cx: &TestAppContext| {
        store.read_with(cx, |store, cx| store.preview_place("one", cx))
    };
    let mut settle =
        |desk_cx: &mut TestAppContext,
         laptop_cx: &mut TestAppContext,
         what: &str,
         ready: &mut dyn FnMut(&mut TestAppContext, &mut TestAppContext) -> bool| {
            loop {
                desk_driver.step(desk_cx);
                laptop_driver.step(laptop_cx);
                if ready(desk_cx, laptop_cx) {
                    return;
                }
                // Both clients share one host, on this thread.
                desk_driver.wait_for_host(what);
            }
        };

    // The desk shows the preview first, as its panel would.
    settle(desk_cx, laptop_cx, "the desk renders", &mut |desk_cx, _| {
        desk.update(desk_cx, |store, _| store.claim_preview("one"));
        place(&desk, desk_cx) == PreviewPlace::Here
    });
    let url = "http://localhost:5173/".to_string();
    desk.update(desk_cx, |store, cx| {
        store.set_preview_url("one", url.clone(), cx)
    });
    settle(
        desk_cx,
        laptop_cx,
        "the laptop sees where the page is",
        &mut |_, laptop_cx| {
            laptop.update(laptop_cx, |store, _| store.claim_preview("one"));
            place(&laptop, laptop_cx) == PreviewPlace::Elsewhere("Desk".into())
                && laptop.read_with(laptop_cx, |store, _| store.preview_url("one"))
                    == Some(url.clone())
        },
    );
    assert!(released.borrow().is_empty());

    laptop.update(laptop_cx, |store, cx| store.show_preview_here("one", cx));
    assert_eq!(place(&laptop, laptop_cx), PreviewPlace::Here);
    settle(
        desk_cx,
        laptop_cx,
        "the desk lets its page go",
        &mut |desk_cx, _| {
            place(&desk, desk_cx) == PreviewPlace::Elsewhere("Laptop".into())
                && !released.borrow().is_empty()
        },
    );
    assert_eq!(*released.borrow(), ["one"]);
    assert_eq!(place(&laptop, laptop_cx), PreviewPlace::Here);
    assert_eq!(
        laptop.read_with(laptop_cx, |store, _| store.preview_url("one")),
        Some(url),
        "the page reopens where it was"
    );

    let link = desk_driver.link.clone();
    desk_driver
        .await_with_events(desk_cx, link.command(Command::ShutdownAllAndFlush))
        .unwrap();
    desk_driver.link.close();
    laptop_driver.link.close();
    drop((desk_driver, laptop_driver));
    host.to_host.close();
    host.from_host.close();
    host.stopped.recv_blocking().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[gpui::test]
fn reconnect_replays_current_index_fingerprint_and_retains_replicas_until_baselines_arrive(
    cx: &mut TestAppContext,
) {
    use tcode_client::ConnectionState;
    let root = scratch_root("baseline-replay");
    let disk = SessionStore::open_at(root.clone()).unwrap();
    disk.upsert_project(&project_at("p", &root)).unwrap();
    disk.upsert_meta(&thread(&root, "one", "p", None)).unwrap();
    let host = test_host(disk);
    let (to_host, requests) = async_channel::unbounded::<String>();
    let replayed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = replayed.clone();
    let host_sender = host.to_host.clone();
    let forwarder = std::thread::spawn(move || {
        while let Ok(line) = requests.recv_blocking() {
            recorded
                .lock()
                .unwrap()
                .push(tcode_protocol::decode_client_line(&line).unwrap());
            if host_sender.send_blocking(line).is_err() {
                break;
            }
        }
        host_sender.close();
    });
    let link = tcode_client::HostLink::new(to_host, host.from_host.clone());
    let mut driver = LocalHostDriver::from_link(link);
    let workspace = driver.workspace(cx);
    workspace.update(cx, |store, cx| store.show_alone("one".into(), cx));
    driver.wait_until(cx, "selected thread baseline", |cx| {
        workspace.read_with(cx, |store, _| store.baseline_ready())
    });
    let link = driver.link.clone();
    driver
        .await_with_events(
            cx,
            link.command(Command::RenameSession {
                session_id: "one".into(),
                title: "Updated live".into(),
            }),
        )
        .unwrap();
    driver.wait_until(cx, "live index upsert", |cx| {
        workspace.read_with(cx, |store, _| {
            store.index_replica.0[0].title == "Updated live"
        })
    });
    let fingerprint = workspace.read_with(cx, |store, _| store.cached_index_fingerprint().unwrap());
    replayed.lock().unwrap().clear();
    workspace.update(cx, |store, cx| {
        let state = ConnectionState::Reconnecting {
            attempt: 2,
            reason: None,
        };
        store.host.set_connection_state(state.clone());
        store.apply_connection_state(state);
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::Scope,
                event: ServerEvent::ScopeSnapshot {
                    scope: tcode_protocol::Scope::Full,
                    host_unix_ms: crate::time::now_millis(cx),
                },
            },
            cx,
        );
        assert!(!store.baseline_ready());
        let state = ConnectionState::Syncing { path: None };
        store.host.set_connection_state(state.clone());
        store.apply_connection_state(state);
        assert!(!store.threads_loading(), "cached list remains visible");
        assert!(
            !store.chat_loading_for(store.sole_thread().as_deref(), cx),
            "cached thread remains visible"
        );
        assert_eq!(store.sole_thread().as_deref(), Some("one"));
    });
    driver.wait_until(cx, "correlated reconnect baselines", |cx| {
        workspace.read_with(cx, |store, _| store.connection_state().is_connected())
    });
    let subscriptions = replayed.lock().unwrap();
    let replay = subscriptions
        .iter()
        .find_map(|request| match &request.payload {
            tcode_protocol::ClientPayload::Subscribe(subscription)
                if subscription.topic == Topic::Index =>
            {
                Some(subscription)
            }
            _ => None,
        })
        .expect("replayed index subscription");
    assert_eq!(replay.known_index.as_ref(), Some(&fingerprint));
    drop(subscriptions);
    let reconnecting = ConnectionState::Reconnecting {
        attempt: 3,
        reason: None,
    };
    let syncing = ConnectionState::Syncing { path: None };
    driver.link.set_connection_state(reconnecting.clone());
    driver.link.set_connection_state(syncing.clone());
    let events = driver.link.events();
    let mut deferred_events = Vec::new();
    let scope = smol::block_on(async {
        let mut scope = None;
        let mut unchanged = false;
        loop {
            if unchanged && let Some(scope) = scope.take() {
                break scope;
            }
            let mut next = std::pin::pin!(events.recv());
            let event = std::future::poll_fn(|task_cx| {
                let _ = driver.pump.as_mut().unwrap().as_mut().poll(task_cx);
                next.as_mut().poll(task_cx)
            })
            .await
            .unwrap();
            if matches!(event.event, ServerEvent::ScopeSnapshot { .. }) {
                scope = Some(event);
            } else {
                if let ServerEvent::IndexUnchanged {
                    fingerprint: received,
                    ..
                } = &event.event
                {
                    assert_eq!(received, &fingerprint);
                    unchanged = true;
                }
                deferred_events.push(event);
            }
        }
    });
    assert!(matches!(scope.event, ServerEvent::ScopeSnapshot { .. }));
    workspace.update(cx, |store, cx| {
        store.receive(scope, cx);
        assert!(
            !store.baseline_ready(),
            "Scope alone cannot make old baselines current"
        );
        store.apply_connection_state(reconnecting);
        store.apply_connection_state(syncing);
        assert!(
            !store.baseline_ready(),
            "other current baselines are still pending"
        );
        assert!(
            !store.chat_loading_for(store.sole_thread().as_deref(), cx),
            "cached thread remains visible"
        );
        for event in deferred_events {
            store.receive(event, cx);
        }
    });
    driver.wait_until(cx, "baselines after delayed state notifications", |cx| {
        workspace.read_with(cx, |store, _| store.connection_state().is_connected())
    });
    workspace.read_with(cx, |store, _| {
        assert_eq!(
            store.cached_index_fingerprint().as_ref(),
            Some(&fingerprint)
        );
        assert_eq!(store.index_replica.0[0].title, "Updated live");
    });
    driver.shutdown(cx, &host);
    driver.link.close();
    forwarder.join().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

/// A conversation left with its drawer open but whose shell has exited
/// comes back with the drawer closed, as the host has it, although this
/// client last showed it open.
#[gpui::test]
fn a_conversation_whose_terminal_exited_comes_back_with_its_drawer_closed(cx: &mut TestAppContext) {
    let root = scratch_root("terminal-drawer-reload");
    let disk = SessionStore::open_at(root.clone()).expect("open test store");
    disk.upsert_project(&project_at("p", &root))
        .expect("persist project");
    for meta in [
        thread(&root, "one", "p", None),
        thread(&root, "two", "p", None),
    ] {
        disk.upsert_meta(&meta).expect("persist session");
    }
    let host = test_host(disk);
    let mut driver = LocalHostDriver::new(&host);
    let workspace = driver.workspace(cx);
    let terminals = |cx: &TestAppContext| {
        workspace.read_with(cx, |store, cx| {
            store
                .session_status_for(Some("one"), cx)
                .map(|status| status.terminals.clone())
                .unwrap_or_default()
        })
    };
    let drawer_open = |cx: &TestAppContext| {
        workspace.read_with(cx, |store, _| {
            store
                .panel_state_for(store.sole_thread().as_deref())
                .terminal_open
        })
    };

    workspace.update(cx, |store, cx| store.show_alone("one".into(), cx));
    driver.wait_until(cx, "one on screen", |cx| {
        selected_status(cx, &workspace, "one")
    });
    workspace.update(cx, |store, cx| {
        store.toggle_terminal_panel_for(store.sole_thread().unwrap_or_default(), cx)
    });
    driver.wait_until(cx, "a terminal", |cx| !terminals(cx).is_empty());
    let terminal_id = terminals(cx)[0].id;
    driver.command(
        cx,
        Command::TerminalInput {
            terminal_id,
            bytes: b"exit\r".to_vec(),
        },
    );
    driver.wait_until(cx, "the shell to exit", |cx| terminals(cx)[0].exited);
    assert!(drawer_open(cx), "the drawer stays while its output is read");

    workspace.update(cx, |store, cx| store.show_alone("two".into(), cx));
    driver.wait_until(cx, "two on screen", |cx| {
        selected_status(cx, &workspace, "two")
    });
    workspace.update(cx, |store, cx| store.show_alone("one".into(), cx));
    let loaded = Topic::SessionStatus {
        session_id: "one".into(),
    };
    driver.wait_until(cx, "one loaded again", |cx| {
        selected_status(cx, &workspace, "one")
            && workspace.read_with(cx, |store, _| store.baseline_topics.contains(&loaded))
    });
    assert!(
        !drawer_open(cx),
        "the drawer opened on a conversation with no shell"
    );
    assert!(terminals(cx).is_empty());

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}

#[gpui::test]
fn a_command_the_host_refuses_is_shown_in_the_hosts_words(cx: &mut TestAppContext) {
    let root = scratch_root("refused-command");
    let host = test_host(SessionStore::open_at(root.clone()).expect("open test store"));
    let mut driver = LocalHostDriver::new(&host);
    let workspace = driver.workspace(cx);
    let reported = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    cx.update(|cx| {
        let reported = reported.clone();
        cx.subscribe(&workspace, move |_, event: &super::RuntimeEvent, _| {
            reported.borrow_mut().push(event.clone());
        })
        .detach();
    });

    workspace.update(cx, |store, _| store.set_auto_settle_after_days(Some(0.5)));
    driver.round_trip(cx);
    let refusal = Settings::default()
        .apply(tcode_core::settings::SettingsPatch::AutoSettleAfterDays(
            Some(0.5),
        ))
        .expect_err("the host refuses half a day");
    assert_eq!(
        reported.borrow().as_slice(),
        [super::RuntimeEvent::Error(
            tcode_protocol::RuntimeError::External(refusal.into())
        )]
    );

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}

pub(crate) fn scratch_root(label: &str) -> std::path::PathBuf {
    static NEXT_ROOT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "tcode-{label}-{}-{}-{}",
        std::process::id(),
        tcode_services::store::now_millis(),
        NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

pub(crate) fn project_at(id: &str, root: &std::path::Path) -> Project {
    Project {
        id: id.into(),
        name: id.into(),
        root: root.to_path_buf(),
        quick_chat: false,
        icon_path: None,
        permission_defaults: Default::default(),
        created_at: 0,
    }
}

pub(crate) fn thread(
    root: &std::path::Path,
    id: &str,
    project: &str,
    parent: Option<&str>,
) -> SessionMeta {
    let mut meta = SessionMeta::new(ProviderKind::Codex, root.to_path_buf(), None);
    meta.id = id.into();
    meta.project_id = Some(project.into());
    meta.parent_session_id = parent.map(str::to_string);
    meta
}

fn selected_status(
    cx: &TestAppContext,
    workspace: &gpui::Entity<WorkspaceStore>,
    session_id: &str,
) -> bool {
    workspace.read_with(cx, |store, cx| {
        store
            .sole_status(cx)
            .is_some_and(|status| status.session_id == session_id)
    })
}

/// Archived threads leave the index; the host still has them.
pub(crate) fn archived(
    cx: &TestAppContext,
    workspace: &gpui::Entity<WorkspaceStore>,
    id: &str,
) -> bool {
    workspace.read_with(cx, |store, _| {
        !store.index_replica.0.iter().any(|meta| meta.id == id)
            && store.index_summary.archived_counts.values().sum::<usize>() > 0
    })
}

/// A settings change made before the host has said what this client may
/// do goes into the outbox, for the host to take or refuse.
#[gpui::test]
fn a_settings_change_before_the_scope_arrives_is_queued(cx: &mut TestAppContext) {
    let (to_host, _requests) = async_channel::unbounded();
    let (_replies, from_host) = async_channel::unbounded();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    store.update(cx, |store, _| store.set_word_wrap_diffs(true));
    assert!(link.pending_commands().iter().any(|(_, command)| matches!(
        command,
        Command::PatchSettings {
            patch: tcode_protocol::SettingsPatch::WordWrapDiffs(true)
        }
    )));
    link.close();
}

/// Deleting a thread deletes every thread under it, archived ones too,
/// though the index carries no archived thread: the confirmation counts
/// them all.
#[gpui::test]
fn delete_confirmation_counts_archived_descendants(cx: &mut TestAppContext) {
    let (to_host, requests) = async_channel::unbounded();
    let (replies, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
    let mut pump = std::pin::pin!(link.pump());
    let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
    let root = std::path::Path::new("/project");
    let archived = |id: &str, parent: &str| {
        let mut meta = thread(root, id, "p", Some(parent));
        meta.archived_at = Some(1);
        meta
    };
    store.update(cx, |store, cx| {
        store.receive(
            EventEnvelope {
                request_id: None,
                topic: Topic::Index,
                event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![
                        thread(root, "parent", "p", None),
                        thread(root, "other", "p", None),
                    ],
                    projects: Vec::new(),
                }),
            },
            cx,
        );
    });

    let count = std::rc::Rc::new(std::cell::Cell::new(None));
    let result = count.clone();
    store.update(cx, |store, cx| {
        let task = store.fetch_deletion_count("parent", cx);
        cx.spawn(async move |_, _| result.set(Some(task.await)))
            .detach();
    });
    cx.run_until_parked();
    let query = std::iter::from_fn(|| requests.try_recv().ok())
        .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
        .find(|message| {
            message.payload
                == tcode_protocol::ClientPayload::Query(tcode_protocol::Query::ArchivedSessions)
        })
        .expect("the count fetches the archived threads");
    replies
        .try_send(
            tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                id: query.id,
                result: Ok(tcode_protocol::QueryResponse::ArchivedSessions(
                    tcode_protocol::ArchivedSessions {
                        sessions: vec![
                            archived("child", "parent"),
                            archived("grandchild", "child"),
                        ],
                        worktree_shared: Default::default(),
                        revision: 0,
                    },
                )),
            })
            .unwrap(),
        )
        .unwrap();
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    cx.run_until_parked();
    assert_eq!(count.take(), Some(Ok(3)));

    // Without the archived threads there is no count to state.
    let result = count.clone();
    store.update(cx, |store, cx| {
        let task = store.fetch_deletion_count("parent", cx);
        cx.spawn(async move |_, _| result.set(Some(task.await)))
            .detach();
    });
    cx.run_until_parked();
    let query = std::iter::from_fn(|| requests.try_recv().ok())
        .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
        .find(|message| {
            message.payload
                == tcode_protocol::ClientPayload::Query(tcode_protocol::Query::ArchivedSessions)
        })
        .expect("the count fetches the archived threads");
    replies
        .try_send(
            tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                id: query.id,
                result: Err(tcode_protocol::ProtocolError {
                    code: "store".into(),
                    message: "the index could not be read".into(),
                }),
            })
            .unwrap(),
        )
        .unwrap();
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    cx.run_until_parked();
    assert_eq!(
        count.take(),
        Some(Err("the index could not be read".to_owned()))
    );
    link.close();
}

/// Delete all deletes each archived thread's tree: the confirmation counts
/// the unarchived threads under them, and a thread under another archived
/// thread is neither counted nor deleted twice.
#[gpui::test]
fn delete_all_archived_counts_unarchived_descendants_once(cx: &mut TestAppContext) {
    let root = std::path::Path::new("/project");
    let archived = |id: &str, parent: Option<&str>| {
        let mut meta = thread(root, id, "p", parent);
        meta.archived_at = Some(1);
        meta
    };
    let delete_all = |cx: &mut TestAppContext, index, archived| {
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        // A thread on screen, so the empty-workspace draft does not open.
        store.update(cx, |store, cx| {
            store.show_alone("other".into(), cx);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                        summary: Default::default(),
                        sessions: index,
                        projects: vec![project_at("p", root)],
                    }),
                },
                cx,
            );
            store.hold_archived_sessions(cx);
        });
        cx.run_until_parked();
        let query = std::iter::from_fn(|| requests.try_recv().ok())
            .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
            .find(|message| {
                message.payload
                    == tcode_protocol::ClientPayload::Query(tcode_protocol::Query::ArchivedSessions)
            })
            .expect("the Archived page fetches the archived threads");
        replies
            .try_send(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                    id: query.id,
                    result: Ok(tcode_protocol::QueryResponse::ArchivedSessions(
                        tcode_protocol::ArchivedSessions {
                            sessions: archived,
                            worktree_shared: Default::default(),
                            revision: 0,
                        },
                    )),
                })
                .unwrap(),
            )
            .unwrap();
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();
        let deletion = store
            .read_with(cx, |store, _| store.archived_deletion())
            .expect("the archived threads are held");
        let counts = (deletion.archived, deletion.unarchived);
        store.update(cx, |store, _| store.delete_archived(deletion));
        cx.run_until_parked();
        let deleted: Vec<String> = std::iter::from_fn(|| requests.try_recv().ok())
            .filter_map(
                |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                    tcode_protocol::ClientPayload::Command(Command::DeleteSession {
                        session_id,
                        ..
                    }) => Some(session_id),
                    _ => None,
                },
            )
            .collect();
        link.close();
        (counts, deleted)
    };

    let (counts, deleted) = delete_all(
        cx,
        vec![
            thread(root, "child", "p", Some("parent")),
            thread(root, "other", "p", None),
        ],
        vec![archived("parent", None)],
    );
    assert_eq!((counts, deleted), ((1, 1), vec!["parent".to_string()]));
    let (counts, deleted) = delete_all(
        cx,
        vec![thread(root, "other", "p", None)],
        vec![archived("child", Some("parent")), archived("parent", None)],
    );
    assert_eq!((counts, deleted), ((2, 0), vec!["parent".to_string()]));
}

/// An Orchestrate child archived while on screen hands the workspace to
/// its parent, keeping the parent's client-side records; archiving a thread
/// the user is not viewing must not move them at all.
#[gpui::test]
fn archiving_the_viewed_child_returns_to_its_parent(cx: &mut TestAppContext) {
    let root = scratch_root("archive-to-parent");
    let disk = SessionStore::open_at(root.clone()).expect("open test store");
    disk.upsert_project(&project_at("p", &root))
        .expect("persist project");
    for meta in [
        thread(&root, "parent", "p", None),
        thread(&root, "child", "p", Some("parent")),
        thread(&root, "sibling", "p", None),
    ] {
        disk.upsert_meta(&meta).expect("persist session");
    }
    let host = test_host(disk);
    let mut driver = LocalHostDriver::new(&host);
    let workspace = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });

    workspace.update(cx, |store, cx| store.show_alone("parent".into(), cx));
    driver.wait_until(cx, "parent on screen", |cx| {
        selected_status(cx, &workspace, "parent")
            && workspace.read_with(cx, |store, _| held_window(store, "parent").is_some())
    });
    let parent_cursor = workspace.read_with(cx, |store, _| Some(held_history(store, "parent").end));
    workspace.update(cx, |store, cx| store.show_alone("child".into(), cx));
    driver.wait_until(cx, "child selected", |cx| {
        selected_status(cx, &workspace, "child")
    });

    driver.command(
        cx,
        Command::ArchiveSession {
            session_id: "child".into(),
        },
    );
    driver.wait_until(cx, "child closed", |cx| {
        workspace.read_with(cx, |store, _| !store.is_thread_open("child"))
    });
    place_shown_threads(&workspace, cx);
    workspace.update(cx, |store, cx| assert!(store.show_after_left("child", cx)));
    driver.wait_until(cx, "parent reopened", |cx| {
        selected_status(cx, &workspace, "parent")
    });
    workspace.read_with(cx, |store, _| {
        let events_cursor = store
            .host
            .subscriptions()
            .into_iter()
            .find(|subscription| matches!(subscription.topic, Topic::SessionEvents { .. }))
            .map(|subscription| subscription.after);
        assert_eq!(
            events_cursor,
            Some(parent_cursor),
            "the parent resumes from its cursor, not from nothing"
        );
    });
    // The index and its summary replicate on their own topics: the
    // return to the parent follows the index, the archived count the
    // summary, and nothing orders one before the other.
    driver.wait_until(cx, "child archived", |cx| archived(cx, &workspace, "child"));

    driver.command(
        cx,
        Command::ArchiveSession {
            session_id: "sibling".into(),
        },
    );
    driver.wait_until(cx, "sibling archived", |cx| {
        archived(cx, &workspace, "sibling")
    });
    assert!(
        selected_status(cx, &workspace, "parent"),
        "archiving a background thread moved the user"
    );

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}

/// A pane split beside a thread asks for a draft in the thread's project,
/// and the thread split from stays open for the pane still showing it.
#[gpui::test]
fn a_draft_beside_an_open_thread_leaves_it_open(cx: &mut TestAppContext) {
    let root = scratch_root("draft-beside");
    let disk = SessionStore::open_at(root.clone()).expect("open test store");
    disk.upsert_project(&project_at("p", &root))
        .expect("persist project");
    disk.upsert_meta(&thread(&root, "first", "p", None))
        .expect("persist session");
    let host = test_host(disk);
    let mut driver = LocalHostDriver::new(&host);
    let workspace = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    workspace.update(cx, |store, cx| store.show_alone("first".into(), cx));
    driver.wait_until(cx, "first selected", |cx| {
        selected_status(cx, &workspace, "first")
    });

    let shown = std::rc::Rc::new(std::cell::RefCell::new(None));
    let _shown = cx.update(|cx| {
        let shown = shown.clone();
        cx.subscribe(&workspace, move |_, event: &super::ShowThread, _| {
            *shown.borrow_mut() = Some(event.session_id.clone());
        })
    });
    workspace.update(cx, |store, cx| {
        store.start_draft_beside(Some("first"), cx);
    });
    driver.wait_until(cx, "a draft to show", |_| shown.borrow().is_some());
    let draft = shown.borrow().clone().expect("the draft");
    workspace.update(cx, |store, cx| store.open_thread(&draft, cx));
    driver.wait_until(cx, "a draft in the thread's project", |cx| {
        workspace.read_with(cx, |store, cx| {
            store
                .session_status_for(Some(&draft), cx)
                .is_some_and(|status| status.draft && status.project_id.as_deref() == Some("p"))
        })
    });
    workspace.read_with(cx, |store, _| {
        assert!(store.is_thread_open("first"));
        assert!(
            store
                .host
                .subscribed_topics()
                .contains(&Topic::SessionEvents {
                    session_id: "first".into()
                }),
            "the thread split from still receives its events"
        );
    });

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}

/// Threads opened before the baseline, one per pane on a restored window
/// or a scope's first answer, are all subscribed by it.
#[gpui::test]
fn a_baseline_subscribes_every_open_thread(cx: &mut TestAppContext) {
    let root = scratch_root("baseline-every-thread");
    let disk = SessionStore::open_at(root.clone()).expect("open test store");
    disk.upsert_project(&project_at("p", &root))
        .expect("persist project");
    for id in ["one", "two"] {
        disk.upsert_meta(&thread(&root, id, "p", None))
            .expect("persist session");
    }
    let host = test_host(disk);
    let mut driver = LocalHostDriver::new(&host);
    let workspace = cx.new(|cx| {
        let mut store = WorkspaceStore::new_attached(
            driver.link.clone(),
            WorkspaceAttachment::Local,
            None,
            None,
            cx,
        );
        store.open_thread("one", cx);
        store.open_thread("two", cx);
        store
    });
    driver.wait_until(cx, "both threads' baselines", |cx| {
        workspace.read_with(cx, |store, _| store.baseline_ready())
    });
    workspace.read_with(cx, |store, cx| {
        assert!(store.session_status_for(Some("one"), cx).is_some());
        assert!(store.session_status_for(Some("two"), cx).is_some());
    });
    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}

#[gpui::test]
fn reconnect_preserves_exactly_one_copy_of_each_record(cx: &mut TestAppContext) {
    let root = scratch_root("p4a-reconnect");
    let disk = SessionStore::open_at(root.clone()).unwrap();
    let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
    meta.id = "reconnect".into();
    disk.upsert_meta(&meta).unwrap();
    for (ts, text) in [(1, "one"), (2, "two"), (3, "three")] {
        disk.append_event(
            "reconnect",
            ts,
            &AgentEvent::Warning {
                message: text.into(),
            },
        )
        .unwrap();
    }
    let host = test_host(disk);
    let mut driver = LocalHostDriver::new(&host);
    let workspace = driver.workspace(cx);
    workspace.update(cx, |store, cx| store.show_alone("reconnect".into(), cx));
    driver.wait_until(cx, "three records", |cx| {
        workspace.read_with(cx, |store, _| {
            held_window(store, "reconnect").is_some_and(|held| held.records.len() == 3)
        })
    });
    driver
        .link
        .set_connection_state(tcode_client::ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        });
    driver
        .link
        .set_connection_state(tcode_client::ConnectionState::Connected { path: None });
    driver.command(cx, Command::ClearRelaunchMarker);
    cx.run_until_parked();
    workspace.read_with(cx, |store, _| {
        assert_eq!(held_history(store, "reconnect").records.len(), 3);
    });
    workspace.read_with(cx, |store, _| {
        assert_eq!(
            held_history(store, "reconnect")
                .records
                .iter()
                .map(|record| record.ts)
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(3)]
        );
        let subscription = store
            .host
            .subscriptions()
            .into_iter()
            .find(|sub| matches!(sub.topic, Topic::SessionEvents { .. }))
            .unwrap();
        assert_eq!(subscription.after, Some(3));
    });
    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}

#[gpui::test]
fn index_and_settings_replicas_follow_representative_commands(cx: &mut TestAppContext) {
    let root = scratch_root("tcode-replica-consistency-test");
    let session_store = SessionStore::open_at(root.clone()).expect("open test store");
    let seed_project = Project::from_root(root.join("seed"));
    let mut seed_session = SessionMeta::new(ProviderKind::Codex, seed_project.root.clone(), None);
    seed_session.project_id = Some(seed_project.id.clone());
    let seed_session_id = seed_session.id.clone();
    session_store
        .upsert_project(&seed_project)
        .expect("persist seed project");
    session_store
        .upsert_meta(&seed_session)
        .expect("persist seed session");
    let host = test_host(session_store);
    let mux = tcode_traverse::mux::HostMux::new(host.attachments.clone(), host.from_host.clone());
    let mut driver = attach(&mux);
    let workspace = driver.workspace(cx);
    driver.wait_until(cx, "initial session index", |cx| {
        workspace.read_with(cx, |store, _| {
            store
                .index_replica
                .0
                .iter()
                .any(|meta| meta.id == seed_session_id)
        })
    });
    workspace.read_with(cx, |store, _| {
        assert!(
            store
                .grouped_sessions()
                .iter()
                .any(|group| { group.sessions.iter().any(|meta| meta.id == seed_session_id) })
        );
        assert!(
            store
                .flat_sessions()
                .iter()
                .any(|meta| meta.id == seed_session_id)
        );
        assert!(store.archived_groups().is_empty());
    });

    // The host validates a project root against its own filesystem, so this
    // directory has to exist before it will accept it.
    let created_root = root.join("created");
    std::fs::create_dir_all(&created_root).unwrap();
    driver.command(cx, Command::CreateProject { root: created_root });
    driver.command(
        cx,
        Command::ArchiveSession {
            session_id: seed_session_id.clone(),
        },
    );
    let mut settings = workspace.read_with(cx, |store, _cx| store.settings());
    settings.word_wrap_diffs = !settings.word_wrap_diffs;
    let expected_word_wrap = settings.word_wrap_diffs;
    driver.command(
        cx,
        Command::PatchSettings {
            patch: tcode_protocol::SettingsPatch::WordWrapDiffs(settings.word_wrap_diffs),
        },
    );
    driver.wait_until(cx, "index and settings replicas", |cx| {
        workspace.read_with(cx, |store, _| {
            store.index_replica.1.len() == 2
                && !store
                    .index_replica
                    .0
                    .iter()
                    .any(|meta| meta.id == seed_session_id)
                && store.settings_replica.word_wrap_diffs == expected_word_wrap
        })
    });
    let link = driver.link.clone();
    let tcode_protocol::QueryResponse::ArchivedSessions(archived) = driver
        .await_with_events(cx, link.query(tcode_protocol::Query::ArchivedSessions))
        .expect("archived threads")
    else {
        panic!("archived threads")
    };
    assert!(
        archived
            .sessions
            .iter()
            .any(|meta| meta.id == seed_session_id)
    );

    workspace.read_with(cx, |store, _| {
        assert!(
            !store
                .grouped_sessions()
                .iter()
                .any(|group| { group.sessions.iter().any(|meta| meta.id == seed_session_id) })
        );
        assert!(
            !store
                .flat_sessions()
                .iter()
                .any(|meta| meta.id == seed_session_id)
        );
        assert_eq!(
            store
                .project_summary(&seed_project.id)
                .map(|(_, count)| count),
            Some(1),
            "the archived thread still counts toward its project"
        );
    });

    // A client that attaches now is given the host's index and settings
    // as they are, and the one that followed every change holds the
    // same. Credential discovery can change settings after a command's
    // reply, which both hear.
    let mut fresh_driver = attach(&mux);
    let fresh = fresh_driver.workspace(cx);
    let replicas = |store: &gpui::Entity<WorkspaceStore>, cx: &TestAppContext| {
        store.read_with(cx, |store, _| {
            (
                serde_json::to_value(&store.index_replica.0).unwrap(),
                serde_json::to_value(&store.index_replica.1).unwrap(),
                serde_json::to_value(&store.settings_replica).unwrap(),
            )
        })
    };
    loop {
        driver.step(cx);
        fresh_driver.step(cx);
        if replicas(&workspace, cx) == replicas(&fresh, cx) {
            break;
        }
        driver.wait_for_host("index and settings equal to a fresh client's");
    }

    shutdown_attached(cx, &host, vec![driver, fresh_driver]);
    let _ = std::fs::remove_dir_all(root);
}

#[gpui::test]
fn session_status_replica_matches_live_after_queue_change(cx: &mut TestAppContext) {
    let root = scratch_root("tcode-session-status-replica-consistency-test");
    let session_store = SessionStore::open_at(root.clone()).expect("open test store");
    let meta = SessionMeta::new(ProviderKind::Codex, root.join("worktree"), None);
    let session_id = meta.id.clone();
    session_store.upsert_meta(&meta).expect("persist session");

    let host = launching_host(
        session_store,
        tcode_runtime::app::scripted_provider(ProviderKind::Codex).launcher,
    );
    let mux = tcode_traverse::mux::HostMux::new(host.attachments.clone(), host.from_host.clone());
    let mut driver = attach(&mux);
    let workspace = driver.workspace(cx);
    workspace.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
    driver.wait_until(cx, "selected session status", |cx| {
        workspace.read_with(cx, |store, cx| {
            store
                .sole_status(cx)
                .is_some_and(|status| status.session_id == session_id)
        })
    });
    driver.command(
        cx,
        Command::SendTurn {
            session_id: session_id.clone(),
            text: "queued for replication".into(),
            attachment_paths: Vec::new(),
        },
    );
    driver.command(
        cx,
        Command::AddReviewComment {
            session_id: session_id.clone(),
            comment: ReviewComment::new(
                "src/lib.rs".into(),
                4,
                4,
                ReviewSide::New,
                "Replicated review draft".into(),
                "+changed".into(),
                "turn:0".into(),
                "Turn 1".into(),
                0,
                1,
            ),
        },
    );
    // A client that opens the thread now is given its status as the host
    // holds it. The scripted provider keeps moving the host (delivery,
    // steering support) after the queue change, which both clients hear,
    // so the two are compared once they agree.
    let mut fresh_driver = attach(&mux);
    let fresh = fresh_driver.workspace(cx);
    fresh.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
    loop {
        driver.step(cx);
        fresh_driver.step(cx);
        let replica = workspace.read_with(cx, |store, cx| store.sole_status(cx).cloned());
        let given = fresh.read_with(cx, |store, cx| store.sole_status(cx).cloned());
        if replica.as_ref().is_some_and(|status| {
            status.queued_messages.len() == 1
                && status.review_comment_drafts.len() == 1
                && given.as_ref() == Some(status)
        }) {
            break;
        }
        driver.wait_for_host("the status a client opening the thread now is given");
    }
    let replica = workspace.read_with(cx, |store, cx| {
        store
            .sole_status(cx)
            .cloned()
            .expect("session status replica")
    });

    assert_eq!(replica.queued_messages.len(), 1);
    assert_eq!(replica.queued_messages[0].text, "queued for replication");
    assert_eq!(replica.review_comment_drafts.len(), 1);
    assert_eq!(
        workspace.read_with(cx, |store, cx| store
            .review_comments_for(store.sole_thread().as_deref(), cx)),
        replica.review_comment_drafts
    );

    shutdown_attached(cx, &host, vec![driver, fresh_driver]);
    let _ = std::fs::remove_dir_all(root);
}

#[gpui::test]
fn rewind_prefills_reach_only_their_client_and_stay_keyed_to_parked_sessions(
    cx: &mut TestAppContext,
) {
    use crate::chat::tests::{record_thread, thread_meta};
    let root = scratch_root("tcode-native-rewind-replica-test");
    let session_store = SessionStore::open_at(root.clone()).expect("open test store");
    // Two threads of a harness that rewinds, each with two turns it
    // checkpointed.
    let (first, second) = (
        thread_meta(Some(ProviderKind::ClaudeCode)),
        thread_meta(Some(ProviderKind::ClaudeCode)),
    );
    for meta in [&first, &second] {
        record_thread(
            &session_store,
            meta,
            (0..2).flat_map(|turn| {
                [
                    AgentEvent::TurnStarted {
                        turn_id: turn.to_string(),
                    },
                    AgentEvent::TurnCheckpoint {
                        turn_id: turn.to_string(),
                        checkpoint_id: format!("checkpoint-{turn}"),
                    },
                    AgentEvent::TurnCompleted {
                        turn_id: turn.to_string(),
                        status: TurnStatus::Completed,
                        usage: Some(agent::TokenUsage {
                            used_tokens: Some(1_000),
                            ..Default::default()
                        }),
                    },
                ]
            }),
        );
    }
    let scripts = tcode_runtime::app::Scripts::default();
    let host = launching_host(session_store, scripts.launcher());
    let mut driver = LocalHostDriver::new(&host);
    let workspace = driver.workspace(cx);
    // The two threads are shown side by side, so both statuses reach
    // this client. A rewind is made from the host's own fold of a
    // thread's log; a status carries the context its turns used once
    // the host has folded them.
    workspace.update(cx, |store, cx| {
        store.show_alone(first.id.clone(), cx);
        store.open_thread(&second.id, cx);
    });
    driver.wait_until(cx, "both threads folded on the host", |cx| {
        workspace.read_with(cx, |store, cx| {
            [&first.id, &second.id].into_iter().all(|id| {
                store
                    .session_status_for(Some(id), cx)
                    .is_some_and(|status| status.session_id == *id && status.usage.is_some())
            })
        })
    });
    let rewound_to = |session_id: &str, checkpoint: &str, cx: &TestAppContext| {
        workspace.read_with(cx, |store, cx| {
            store
                .with_timeline_for(
                    Some(session_id),
                    |timeline| {
                        timeline.entries.iter().any(|entry| {
                            matches!(&entry.content, EntryContent::Rewound(rewound)
                                    if rewound.checkpoint_id == checkpoint)
                        })
                    },
                    cx,
                )
                .unwrap_or_default()
        })
    };
    // The harness rewinds what the host asks it to and gives the prompt
    // back.
    let answer = |driver: &mut LocalHostDriver,
                  cx: &mut TestAppContext,
                  provider: &tcode_runtime::app::ScriptedProvider,
                  prefill: &str| {
        let (checkpoint_id, mode) = loop {
            if let agent::SessionCommand::Rewind {
                checkpoint_id,
                mode,
            } = driver.recv(cx, &provider.commands)
            {
                break (checkpoint_id, mode);
            }
        };
        provider
            .events
            .try_send(AgentEvent::RewindCompleted {
                checkpoint_id,
                mode,
                prefill: Some(prefill.into()),
                prefill_attachments: Vec::new(),
                later_history: agent::LaterHistory::Truncated,
                files: None,
                incomplete: None,
                undo_token: None,
            })
            .unwrap();
    };

    // This client rewinds the first thread's second message.
    let provider = scripts.script(ProviderKind::ClaudeCode);
    workspace.update(cx, |store, _| {
        store.rewind_turn_for(
            first.id.clone(),
            1,
            None,
            None,
            agent::RewindMode::Conversation,
        )
    });
    answer(&mut driver, cx, &provider, "first parked prefill");
    driver.wait_until(cx, "the first thread's prompt back", |cx| {
        workspace.read_with(cx, |store, cx| {
            store.composer_text_waiting_for(Some(&first.id), cx)
        })
    });
    // Another client rewinds it further: that prompt is the other
    // client's to put back.
    driver.command(
        cx,
        Command::RewindTurn {
            session_id: first.id.clone(),
            turn: 0,
            steer: None,
            checkpoint_id: None,
            mode: agent::RewindMode::Conversation,
            client_id: Some("another-client".into()),
        },
    );
    answer(&mut driver, cx, &provider, "asked elsewhere");
    driver.wait_until(cx, "the other client's rewind", |cx| {
        rewound_to(&first.id, "checkpoint-0", cx)
    });
    driver.round_trip(cx);
    // This client rewinds the second thread, shown beside the first.
    let provider = scripts.script(ProviderKind::ClaudeCode);
    workspace.update(cx, |store, _| {
        store.rewind_turn_for(
            second.id.clone(),
            1,
            None,
            None,
            agent::RewindMode::Conversation,
        )
    });
    answer(&mut driver, cx, &provider, "second parked prefill");
    driver.wait_until(cx, "the second thread's prompt back", |cx| {
        workspace.read_with(cx, |store, cx| {
            store.composer_text_waiting_for(Some(&second.id), cx)
        })
    });

    for (session_id, text) in [
        (&first.id, "first parked prefill"),
        (&second.id, "second parked prefill"),
    ] {
        assert_eq!(
            workspace.update(cx, |store, cx| store
                .take_native_rewind_prefill_for(Some(session_id), cx)),
            Some(crate::store::RewindPrefill {
                text: text.into(),
                attachments: Vec::new(),
            }),
            "each thread's composer gets the prompt this client asked for there"
        );
    }

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
}

/// A draft's client state follows its project, so deleting the project
/// takes the draft's `ConversationUiState` with it instead of stranding an
/// entry keyed by the transient draft session id.
#[gpui::test]
fn removing_a_project_closes_its_draft_and_leaves_other_threads_shown(cx: &mut TestAppContext) {
    for viewing_draft in [false, true] {
        let root = scratch_root("remove-project-draft-ui");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        for project in ["doomed", "kept"] {
            disk.upsert_project(&project_at(project, &root))
                .expect("persist project");
        }
        disk.upsert_meta(&thread(&root, "kept-thread", "kept", None))
            .expect("persist session");
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);

        workspace.update(cx, |store, cx| {
            store.start_draft("doomed".into(), root.clone(), cx)
        });
        driver.wait_until(cx, "draft for the doomed project", |cx| {
            workspace.read_with(cx, |store, cx| {
                store.sole_status(cx).is_some_and(|status| {
                    status.draft && status.project_id.as_deref() == Some("doomed")
                })
            })
        });

        if !viewing_draft {
            workspace.update(cx, |store, cx| store.show_alone("kept-thread".into(), cx));
            driver.wait_until(cx, "kept thread selected", |cx| {
                selected_status(cx, &workspace, "kept-thread")
            });
        }
        workspace.update(cx, |store, _| store.delete_project("doomed".into()));
        driver.wait_until(cx, "project removed", |cx| {
            workspace.read_with(cx, |store, _| {
                !store
                    .index_replica
                    .1
                    .iter()
                    .any(|project| project.id == "doomed")
            })
        });
        if viewing_draft {
            assert_eq!(
                workspace.read_with(cx, |store, _| store.sole_thread()),
                None,
                "the deleted project's draft stayed open"
            );
        } else {
            assert!(
                selected_status(cx, &workspace, "kept-thread"),
                "deleting a background project moved the user"
            );
        }

        driver.shutdown(cx, &host);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Committing a draft into a real session moves its client state from the
/// project-draft key to the new thread key, so the user keeps the panel
/// layout they were typing in.
#[gpui::test]
fn committing_a_draft_carries_its_conversation_state_to_the_thread(cx: &mut TestAppContext) {
    let root = scratch_root("commit-draft-ui");
    let disk = SessionStore::open_at(root.clone()).expect("open test store");
    disk.upsert_project(&project_at("p", &root))
        .expect("persist project");
    let scripts = tcode_runtime::app::Scripts::default();
    let host = launching_host(disk, scripts.launcher());
    let mut driver = LocalHostDriver::new(&host);
    let workspace = driver.workspace(cx);

    workspace.update(cx, |store, cx| {
        store.start_draft("p".into(), root.clone(), cx)
    });
    driver.wait_until(cx, "draft for p", |cx| {
        workspace.read_with(cx, |store, cx| {
            store
                .sole_status(cx)
                .is_some_and(|status| status.draft && status.project_id.as_deref() == Some("p"))
        })
    });
    let draft_id = workspace
        .read_with(cx, |store, _| store.sole_thread())
        .expect("draft selected");
    workspace.update(cx, |store, _| {
        store
            .conversation_ui
            .get_mut(&ConversationDestination::ProjectDraft("p".into()))
            .expect("draft conversation state")
            .right_panel_open = true;
    });

    let provider = workspace.read_with(cx, |store, cx| store.sole_status(cx).unwrap().provider);
    let _provider = scripts.script(provider);
    driver.command(
        cx,
        Command::SendTurn {
            session_id: draft_id.clone(),
            text: "first turn".into(),
            attachment_paths: Vec::new(),
        },
    );
    driver.wait_until(cx, "draft committed", |cx| {
        workspace.read_with(cx, |store, cx| {
            store
                .sole_status(cx)
                .is_some_and(|status| !status.draft && status.session_id == draft_id)
        })
    });
    workspace.read_with(cx, |store, _| {
        assert!(
            store.panel_state_for(Some(&draft_id)).right_panel_open,
            "the committed thread lost the state the user left in its draft"
        );
    });

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}
