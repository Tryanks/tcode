use gpui::{TestAppContext, VisualTestContext, size};
use tcode_protocol::SettingsPatch;
use tcode_runtime::pipe::{HostServices, spawn_host};
use tcode_services::store::SessionStore;

use super::*;
use crate::store::WorkspaceAttachment;

fn capabilities(
    preview_backend: bool,
    hosting: bool,
    local_permissions: bool,
    remote_attachment: bool,
) -> SettingsCapabilities {
    SettingsCapabilities {
        preview_backend,
        hosting,
        local_permissions,
        remote_attachment,
    }
}

/// A full-scope store whose host is scripted by the test: what the store
/// sends arrives on the first channel, and lines sent on the second
/// arrive from the host while the returned pump runs.
fn scripted_store(
    attachment: WorkspaceAttachment,
    cx: &mut TestAppContext,
) -> (
    Entity<WorkspaceStore>,
    async_channel::Receiver<String>,
    async_channel::Sender<String>,
    gpui::Task<()>,
) {
    cx.update(crate::test_client::init);
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let pump_link = link.clone();
    let pump = cx.background_executor.spawn(async move {
        pump_link.pump().await;
    });
    let store = cx.new(|cx| WorkspaceStore::new_attached(link, attachment, None, None, cx));
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    (store, outgoing, incoming, pump)
}

#[gpui::test]
fn archived_delete_uses_response_worktree_sharing_and_preserves_the_cleanup_choice(
    cx: &mut TestAppContext,
) {
    use tcode_protocol::{
        ArchivedSessions, ClientPayload, Command, HostMessage, Query, QueryResponse,
        decode_client_line, encode_line,
    };
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let (store, outgoing, incoming, _pump) = scripted_store(WorkspaceAttachment::Local, cx);
    store.update(cx, |store, cx| store.show_alone("live".into(), cx));
    let window_state = cx.new(|_| WindowState::default());
    let mut page = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
        page = Some(view.clone());
        gpui_base::Root::new(view, window, cx)
    });
    let page = page.unwrap();
    page.update(cx, |page, cx| page.select_section(Section::Archived, cx));
    cx.run_until_parked();
    let mut archived_query = None;
    while let Ok(line) = outgoing.try_recv() {
        let message = decode_client_line(&line).unwrap();
        match message.payload {
            ClientPayload::Query(Query::ArchivedSessions) => archived_query = Some(message.id),
            ClientPayload::Command(_) => incoming
                .try_send(
                    encode_line(&HostMessage::Ack {
                        id: message.id,
                        result: Ok(tcode_protocol::CommandResponse::Unit),
                    })
                    .unwrap(),
                )
                .unwrap(),
            _ => {}
        }
    }
    let sessions = ["shared", "remove", "keep"]
        .into_iter()
        .map(|id| {
            let mut meta = tcode_core::project::SessionMeta::new(
                agent::ProviderKind::Codex,
                std::path::PathBuf::from(format!("/archive/worktree/{id}")),
                None,
            );
            meta.id = id.into();
            meta.title = id.into();
            meta.archived_at = Some(1);
            meta.worktree = Some(tcode_core::project::WorktreeInfo {
                root_project_path: "/archive/project".into(),
                base: "main".into(),
                branch: format!("tcode/archive/{id}"),
            });
            meta
        })
        .collect();
    incoming
        .try_send(
            encode_line(&HostMessage::QueryResult {
                id: archived_query.expect("opening Archived queries the host"),
                result: Ok(QueryResponse::ArchivedSessions(ArchivedSessions {
                    sessions,
                    worktree_shared: std::collections::HashSet::from(["shared".into()]),
                    revision: 0,
                })),
            })
            .unwrap(),
        )
        .unwrap();
    cx.run_until_parked();
    for (id, cleanup, remove_worktree) in [
        ("shared", false, false),
        ("remove", true, true),
        ("keep", true, false),
    ] {
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                page.confirm_delete_archived(id, id, window, cx)
            });
            _ = window.draw(cx);
            window.dispatch_action(
                Box::new(gpui_base::actions::Confirm { secondary: false }),
                cx,
            );
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        if cleanup {
            assert!(outgoing.is_empty(), "delete waits for the cleanup choice");
            cx.update(|window, cx| {
                if remove_worktree {
                    window.dispatch_action(
                        Box::new(gpui_base::actions::Confirm { secondary: false }),
                        cx,
                    );
                } else {
                    window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
                }
            });
            cx.run_until_parked();
        }
        let request = decode_client_line(&outgoing.try_recv().expect("delete command")).unwrap();
        assert_eq!(
            request.payload,
            ClientPayload::Command(Command::DeleteSession {
                session_id: id.into(),
                remove_worktree,
            })
        );
        incoming
            .try_send(
                encode_line(&HostMessage::Ack {
                    id: request.id,
                    result: Ok(tcode_protocol::CommandResponse::Unit),
                })
                .unwrap(),
            )
            .unwrap();
        cx.run_until_parked();
    }
}

/// A client attached to another machine shows that machine's storage
/// migration in Maintenance only while the host says one runs: nothing
/// before, a card while it runs, nothing once it is done; and it stops
/// following once Maintenance closes.
#[gpui::test]
fn maintenance_shows_the_hosts_storage_migration_only_while_it_runs(cx: &mut TestAppContext) {
    use tcode_protocol::{
        ClientPayload, EventEnvelope, HostMessage, ServerEvent, StorageMigration, Subscription,
        Topic, decode_client_line, encode_line,
    };
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let attachment = WorkspaceAttachment::Remote {
        host_id: "studio".into(),
        host_name: "Studio".into(),
    };
    let (store, outgoing, incoming, _pump) = scripted_store(attachment, cx);
    let window_state = cx.new(|_| WindowState::default());
    let mut page = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
        page = Some(view.clone());
        gpui_base::Root::new(view, window, cx)
    });
    let page = page.unwrap();
    let follows = |outgoing: &async_channel::Receiver<String>, subscribe: bool| {
        std::iter::from_fn(|| outgoing.try_recv().ok()).find_map(|line| {
            let message = decode_client_line(&line).unwrap();
            match message.payload {
                ClientPayload::Subscribe(Subscription {
                    topic: Topic::StorageMigration,
                    ..
                }) if subscribe => Some(message.id),
                ClientPayload::Unsubscribe(Subscription {
                    topic: Topic::StorageMigration,
                    ..
                }) if !subscribe => Some(message.id),
                _ => None,
            }
        })
    };
    let send = |request_id, migration| {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id,
                    topic: Topic::StorageMigration,
                    event: ServerEvent::StorageMigrationReplaced(migration),
                }))
                .unwrap(),
            )
            .unwrap();
    };
    let shown = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        cx.run_until_parked();
        crate::test_frames::draw_settled(cx);
        cx.debug_bounds("storage-migration").is_some()
    };

    page.update(cx, |page, cx| page.select_section(Section::Maintenance, cx));
    cx.run_until_parked();
    let request = follows(&outgoing, true).expect("Maintenance follows the migration");
    send(Some(request), None);
    assert!(!shown(cx), "no card while no migration runs");

    send(
        None,
        Some(StorageMigration {
            moved: 30,
            total: 120,
            waiting_for_space: false,
        }),
    );
    assert!(shown(cx), "a card while the host moves threads");

    send(None, None);
    assert!(!shown(cx), "the card leaves once the move is done");

    cx.update(|window, _| window.activate_window());
    page.update(cx, |page, cx| page.select_section(Section::General, cx));
    cx.run_until_parked();
    assert!(
        follows(&outgoing, false).is_some(),
        "only Maintenance follows it"
    );
}

/// Maintenance on a client attached to another machine shows that
/// machine's disk usage as its host measured it: a compact loading state
/// before the first measurement, then Tcode's total and a row for each
/// provider the host listed, which stay while the host measures again.
#[gpui::test]
fn maintenance_shows_the_hosts_disk_usage_and_keeps_it_while_measuring_again(
    cx: &mut TestAppContext,
) {
    use agent::ProviderKind;
    use tcode_protocol::{
        AccountDiskUsage, ClientPayload, DiskUsage, DiskUsageState, EventEnvelope, HostMessage,
        ProviderDiskUsage, ServerEvent, Subscription, TcodeDiskUsage, Topic, decode_client_line,
        encode_line,
    };
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let attachment = WorkspaceAttachment::Remote {
        host_id: "studio".into(),
        host_name: "Studio".into(),
    };
    let (store, outgoing, incoming, _pump) = scripted_store(attachment, cx);
    let window_state = cx.new(|_| WindowState::default());
    let mut page = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
        page = Some(view.clone());
        gpui_base::Root::new(view, window, cx)
    });
    let page = page.unwrap();
    // The spinner of a measurement running rests only with motion reduced.
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let send = |request_id, state| {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id,
                    topic: Topic::DiskUsage,
                    event: ServerEvent::DiskUsageReplaced(state),
                }))
                .unwrap(),
            )
            .unwrap();
    };
    let shown = |cx: &mut VisualTestContext, selectors: &[&'static str]| {
        cx.run_until_parked();
        cx.run_until_parked();
        crate::test_frames::draw_settled(cx);
        selectors
            .iter()
            .filter(|selector| cx.debug_bounds(selector).is_some())
            .copied()
            .collect::<Vec<_>>()
    };
    let account = |profile_id: &str, dir: &str, bytes| AccountDiskUsage {
        profile_id: profile_id.into(),
        dirs: vec![dir.into()],
        bytes,
        parts: Vec::new(),
    };
    let usage = DiskUsage {
        measured_at: 0,
        tcode: TcodeDiskUsage {
            dir: "~/.tcode".into(),
            conversations: 40 << 30,
            images: 70 << 20,
            worktrees: 0,
            other: 5 << 20,
        },
        providers: vec![
            ProviderDiskUsage {
                provider: ProviderKind::Codex,
                bytes: 30 << 30,
                accounts: vec![account("codex", "~/.codex", 30 << 30)],
            },
            ProviderDiskUsage {
                provider: ProviderKind::ClaudeCode,
                bytes: 6 << 30,
                accounts: vec![
                    account("claude", "~/.claude", 6 << 30),
                    account("work", "~/.tcode/profile-homes/work", 10 << 20),
                ],
            },
        ],
    };
    let selectors = [
        "disk-usage-measuring",
        "disk-usage-remeasuring",
        "disk-usage-tcode",
        "disk-usage-tcode-conversations",
        "disk-usage-tcode-worktrees",
        "disk-usage-provider-codex",
        "disk-usage-provider-claude",
        "disk-usage-provider-pi",
    ];

    page.update(cx, |page, cx| page.select_section(Section::Maintenance, cx));
    cx.run_until_parked();
    let request = std::iter::from_fn(|| outgoing.try_recv().ok())
        .find_map(|line| match decode_client_line(&line).unwrap() {
            message @ tcode_protocol::ClientMessage {
                payload:
                    ClientPayload::Subscribe(Subscription {
                        topic: Topic::DiskUsage,
                        ..
                    }),
                ..
            } => Some(message.id),
            _ => None,
        })
        .expect("Maintenance follows the disk usage");
    send(
        Some(request),
        DiskUsageState {
            last: None,
            measuring: true,
        },
    );
    assert_eq!(shown(cx, &selectors), ["disk-usage-measuring"]);

    send(
        None,
        DiskUsageState {
            last: Some(usage.clone()),
            measuring: false,
        },
    );
    let rows = [
        "disk-usage-tcode",
        "disk-usage-tcode-conversations",
        "disk-usage-tcode-worktrees",
        "disk-usage-provider-codex",
        "disk-usage-provider-claude",
    ];
    assert_eq!(shown(cx, &selectors), rows);

    send(
        None,
        DiskUsageState {
            last: Some(usage),
            measuring: true,
        },
    );
    assert_eq!(
        shown(cx, &selectors),
        [&["disk-usage-remeasuring"][..], &rows[..]].concat()
    );
}

struct CompactSettingsProbe {
    page: Entity<SettingsPage>,
}

impl Render for CompactSettingsProbe {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.page
            .update(cx, |page, cx| page.render_compact_list(cx))
    }
}

#[gpui::test]
fn compact_list_groups_sections_omits_inapplicable_and_folds_advanced(cx: &mut TestAppContext) {
    cx.update(crate::test_client::init);
    let root = crate::store::tests::scratch_root("settings-sections");
    let host = spawn_host(
        SessionStore::open_at(root.clone()).unwrap(),
        HostServices::default(),
    )
    .expect("spawn settings test host");
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    let window_state = cx.new(|_| WindowState::default().with_compact(true));
    let (probe, cx) = cx.add_window_view(|window, cx| {
        let page = cx.new(|cx| {
            let mut page = SettingsPage::new(store.clone(), window_state.clone(), window, cx);
            page.capabilities = capabilities(false, false, false, true);
            page
        });
        CompactSettingsProbe { page }
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(393.), px(852.)));
    // The navigation consumes capability values supplied by platform
    // owners. Exercise each policy row through its rendered section list;
    // local permission eligibility is a policy check, not an OS grant test.
    // `machine_spaces`: a desktop over another machine manages that
    // machine's shared spaces, whether or not it hosts itself.
    for (capabilities, local_permissions, folded, browser, remote, machine_spaces) in [
        (
            capabilities(true, true, true, false),
            true,
            false,
            true,
            true,
            false,
        ),
        (
            capabilities(true, true, true, true),
            false,
            false,
            true,
            true,
            true,
        ),
        (
            capabilities(false, false, false, true),
            false,
            true,
            false,
            false,
            true,
        ),
    ] {
        probe.update(cx, |probe, cx| {
            probe.page.update(cx, |page, cx| {
                page.capabilities = capabilities;
                page.advanced_expanded = None;
                assert_eq!(
                    page.capabilities.can_manage_local_permissions(),
                    local_permissions
                );
                cx.notify();
            });
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            _ = window.draw(cx);
        });
        for selector in [
            "settings-device-caption",
            "settings-machine-caption",
            "settings-nav-general",
            "settings-nav-threads",
            "settings-nav-providers",
            "settings-nav-usage",
            "settings-nav-archived",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{capabilities:?}: {selector}"
            );
        }
        assert_eq!(cx.debug_bounds("settings-nav-browser").is_some(), browser);
        #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
        assert_eq!(cx.debug_bounds("settings-nav-remote").is_some(), remote);
        #[cfg(not(any(feature = "remote-hosting", target_family = "wasm")))]
        let _ = remote;
        #[cfg(feature = "remote-hosting")]
        assert_eq!(
            cx.debug_bounds("settings-nav-machine-spaces").is_some(),
            machine_spaces,
            "{capabilities:?}"
        );
        #[cfg(not(feature = "remote-hosting"))]
        let _ = machine_spaces;
        assert!(
            cx.debug_bounds("settings-advanced-toggle").is_some(),
            "{capabilities:?}: advanced disclosure remains available in either state"
        );
        for advanced in ["settings-nav-computer-use", "settings-nav-maintenance"] {
            assert_eq!(cx.debug_bounds(advanced).is_some(), !folded, "{advanced}");
        }
        if folded {
            let toggle = cx.debug_bounds("settings-advanced-toggle").unwrap();
            cx.simulate_click(toggle.center(), gpui::Modifiers::default());
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.refresh();
                _ = window.draw(cx);
            });
            assert!(cx.debug_bounds("settings-nav-computer-use").is_some());
            assert!(cx.debug_bounds("settings-nav-maintenance").is_some());
            assert!(cx.debug_bounds("settings-nav-browser").is_none());
        }
    }

    // A stale palette/deep-link target is rejected by the same predicate
    // and returns a compact detail route to the root list.
    window_state.update(cx, |state, cx| {
        state.enter_workspace(cx);
        state.open_settings(cx);
        state.go(crate::window_state::Destination::SettingsSection, cx);
    });
    probe.update(cx, |probe, cx| {
        probe
            .page
            .update(cx, |page, cx| page.open_section("browser", cx))
    });
    cx.run_until_parked();
    assert_eq!(
        window_state.read_with(cx, |state, _| state.destination()),
        crate::window_state::Destination::Settings
    );

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
}

/// "Show more" asks for a longer newest-first list, and a new filter asks
/// for the first page of what it matches: the two the host answers.
#[gpui::test]
fn the_activity_list_pages_and_filters_through_the_host(cx: &mut TestAppContext) {
    use tcode_protocol::{
        ClientPayload, ControlActivityEntry, ControlActivityFilter, ControlActivityPage,
        ControlActivityThread, HostMessage, Query, QueryResponse, decode_client_line, encode_line,
    };
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let (store, outgoing, incoming, _pump) = scripted_store(WorkspaceAttachment::Local, cx);
    let window_state = cx.new(|_| WindowState::default());
    let mut page = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
        page = Some(view.clone());
        gpui_base::Root::new(view, window, cx)
    });
    let page = page.unwrap();
    cx.simulate_resize(gpui::size(px(1024.), px(3000.)));
    let asked = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(|line| {
                let message = decode_client_line(&line).unwrap();
                match message.payload {
                    ClientPayload::Query(Query::ControlActivity { limit, filter }) => {
                        Some((message.id, limit, filter))
                    }
                    _ => None,
                }
            })
            .last()
            .expect("the page asked the host")
    };
    let answer = |id, total, cx: &mut VisualTestContext| {
        let entry = ControlActivityEntry {
            target_thread_id: "target".into(),
            target_title: "Target".into(),
            at: 1,
            action: agent::ControlAction {
                actor_thread_id: "actor".into(),
                actor_title: "Actor".into(),
                action: "send_message".into(),
                detail: None,
            },
        };
        incoming
            .try_send(
                encode_line(&HostMessage::QueryResult {
                    id,
                    result: Ok(QueryResponse::ControlActivity(ControlActivityPage {
                        entries: vec![entry],
                        total,
                        threads: vec![ControlActivityThread {
                            id: "target".into(),
                            title: "Target".into(),
                        }],
                        actions: vec!["send_message".into()],
                    })),
                })
                .unwrap(),
            )
            .unwrap();
        cx.run_until_parked();
        crate::test_frames::draw_settled(cx);
    };

    page.update(cx, |page, cx| page.select_section(Section::Control, cx));
    crate::test_frames::draw(cx);
    let (id, limit, filter) = asked(cx);
    assert_eq!((limit, &filter), (50, &ControlActivityFilter::default()));

    answer(id, 1, cx);
    assert!(
        cx.debug_bounds("control-activity-more").is_none(),
        "every match is listed"
    );

    let refresh = cx.debug_bounds("control-activity-refresh").unwrap();
    cx.simulate_click(refresh.center(), Default::default());
    let (id, ..) = asked(cx);
    answer(id, 60, cx);
    let more = cx
        .debug_bounds("control-activity-more")
        .expect("more match");
    cx.simulate_click(more.center(), Default::default());
    let (id, limit, _) = asked(cx);
    assert_eq!(limit, 100);
    answer(id, 60, cx);

    let thread = cx.debug_bounds("control-activity-thread").unwrap();
    cx.simulate_click(thread.center(), Default::default());
    crate::test_frames::draw_settled(cx);
    let option = cx
        .debug_bounds("control-activity-thread-Target")
        .expect("the log's threads are offered");
    cx.simulate_click(option.center(), Default::default());
    let (_, limit, filter) = asked(cx);
    assert_eq!(limit, 50, "a filter starts over at the first page");
    assert_eq!(filter.thread_id.as_deref(), Some("target"));
}

/// A client that renders before its host answers must not present its own
/// defaults as the host's configuration, must adopt the host's values the
/// moment they arrive, and must never overwrite what the user has since
/// typed — no matter how many snapshots follow.
#[gpui::test]
fn editable_settings_wait_for_the_host_and_then_keep_the_user_edit(cx: &mut TestAppContext) {
    let root = crate::store::tests::scratch_root("settings-hydration");
    std::fs::create_dir_all(&root).unwrap();
    let mut settings = tcode_core::settings::Settings::default();
    settings.browser.home_url = Some("https://host.example".into());
    tcode_services::settings::SettingsStore::new(root.clone())
        .save(&settings)
        .expect("seed host settings");
    let host = spawn_host(
        SessionStore::open_at(root.clone()).unwrap(),
        HostServices::default(),
    )
    .expect("spawn settings test host");

    // No blocking seed: this is the portable client's timeline, where the
    // first snapshot lands after the page already exists.
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    let window_state = cx.new(|_| WindowState::default());
    let (page, cx) = cx.add_window_view(|window, cx| {
        SettingsPage::new(store.clone(), window_state.clone(), window, cx)
    });
    let cx: &mut VisualTestContext = cx;

    page.read_with(cx, |page, cx| {
        assert_eq!(
            page.home_url_input.state.read(cx).value(),
            "",
            "an unhydrated field must stay empty rather than show a local default"
        );
    });

    driver.wait_until(cx, "the field to show the host's value", |cx| {
        page.read_with(cx, |page, cx| {
            page.home_url_input.state.read(cx).value() == "https://host.example"
        })
    });

    // The user edits the field: `replace_all` takes the same path typing
    // does, emitting the change the page listens for.
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.home_url_input.state.update(cx, |input, cx| {
                input.replace_all("https://edited", window, cx)
            });
        });
    });
    cx.run_until_parked();

    let link = driver.link.clone();
    driver
        .await_with_events(
            cx,
            link.command(tcode_protocol::Command::PatchSettings {
                patch: SettingsPatch::BrowserHomeUrl(Some("https://elsewhere.example".into())),
            }),
        )
        .expect("patch settings");
    driver.wait_until(cx, "the later settings snapshot", |cx| {
        store.read_with(cx, |store, _| {
            store.settings().browser.home_url.as_deref() == Some("https://elsewhere.example")
        })
    });

    page.read_with(cx, |page, cx| {
        assert_eq!(
            page.home_url_input.state.read(cx).value(),
            "https://edited",
            "a later snapshot must not clobber the field the user is editing"
        );
    });

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
}

/// The new project folder's placeholder is written under the host's
/// home, which the page asks the host for; a failed ask says why and is
/// not made again with every drawing.
#[gpui::test]
fn a_failed_ask_for_the_host_home_says_why_once(cx: &mut TestAppContext) {
    use tcode_protocol::{ClientPayload, HostMessage, Query, decode_client_line, encode_line};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let (store, outgoing, incoming, _pump) = scripted_store(WorkspaceAttachment::Local, cx);
    let window_state = cx.new(|_| WindowState::default());
    let mut page = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
        page = Some(view.clone());
        gpui_base::Root::new(view, window, cx)
    });
    let page = page.unwrap();
    let reported = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let reported = reported.clone();
        cx.subscribe(
            &store,
            move |_, event: &tcode_protocol::RuntimeNotification, _| {
                reported.borrow_mut().push(event.clone());
            },
        )
        .detach();
    });
    let asked_home = |outgoing: &async_channel::Receiver<String>| {
        std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(|line| {
                let message = decode_client_line(&line).unwrap();
                match message.payload {
                    ClientPayload::Query(Query::BrowseDirectories { path }) if path == "~" => {
                        Some(message.id)
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
    };
    let answer = |id, result| {
        incoming
            .try_send(encode_line(&HostMessage::QueryResult { id, result }).unwrap())
            .unwrap();
    };
    let drawn = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        cx.run_until_parked();
        crate::test_frames::draw_settled(cx);
        page.read_with(cx, |page, cx| {
            page.new_project_dir_input
                .state
                .read(cx)
                .presentation()
                .placeholder()
                .to_string()
        })
    };

    page.update(cx, |page, cx| page.select_section(Section::Threads, cx));
    drawn(cx);
    let [first] = asked_home(&outgoing)[..] else {
        panic!("drawing the Projects rows asks for the host's home once");
    };
    answer(
        first,
        Err(tcode_protocol::ProtocolError {
            code: "transport_closed".into(),
            message: "The host went away.".into(),
        }),
    );
    assert_eq!(drawn(cx), "~/TcodeProjects");
    assert_eq!(drawn(cx), "~/TcodeProjects");
    assert!(
        asked_home(&outgoing).is_empty(),
        "drawing does not ask again"
    );
    assert_eq!(
        reported.borrow().as_slice(),
        [tcode_protocol::RuntimeNotification::Error(
            tcode_protocol::RuntimeError::External(
                crate::tr!("sidebar.folders_failed", reason = "The host went away.").into_owned()
            )
        )]
    );
}

/// The attachment limit is taken when the user leaves the field. A value
/// it cannot take is not saved, and the field shows the limit in effect
/// rather than what was typed.
#[gpui::test]
fn an_attachment_limit_the_field_cannot_take_shows_the_one_in_effect(cx: &mut TestAppContext) {
    use tcode_client::host::ClientHost as _;
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::test_client::init);
    let root = crate::store::tests::scratch_root("attachment-limit-field");
    std::fs::create_dir_all(&root).unwrap();
    let device = std::rc::Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
    cx.update(|cx| DevicePreferences::install(device.clone(), cx));
    let (to_host, _outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store =
        cx.new(|cx| WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx));
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    let window_state = cx.new(|_| WindowState::default());
    let mut page = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
        page = Some(view.clone());
        gpui_base::Root::new(view, window, cx)
    });
    let page = page.unwrap();
    cx.update(|window, _| window.activate_window());
    page.update(cx, |page, cx| page.select_section(Section::General, cx));
    crate::test_frames::draw_settled(cx);
    assert!(
        cx.debug_bounds("settings-remote-attachment-limit-row")
            .is_some()
    );
    let in_effect = cx.update(|_, cx| remote_attachment_limit_value(cx));
    let field = page.read_with(cx, |page, _| {
        page.remote_attachment_limit_input.state.clone()
    });
    // What the user types after clearing the field, then leaving it.
    let type_and_leave = |typed: &str, cx: &mut gpui::VisualTestContext| {
        let row = cx
            .debug_bounds("settings-remote-attachment-limit-row")
            .expect("the limit's row is drawn");
        cx.simulate_click(
            gpui::point(row.right() - gpui::px(40.), row.center().y),
            gpui::Modifiers::default(),
        );
        crate::test_frames::draw(cx);
        cx.simulate_keystrokes("end backspace backspace backspace");
        cx.simulate_input(typed);
        crate::test_frames::draw(cx);
        cx.update(|window, cx| window.blur(cx));
        crate::test_frames::draw_settled(cx);
        (
            device
                .load_preferences()
                .unwrap()
                .remote_attachment_limit_mib,
            cx.update(|_, cx| field.read(cx).value().to_string()),
        )
    };
    assert_eq!(type_and_leave("", cx), (None, in_effect));
    assert_eq!(type_and_leave("12", cx), (Some(12), "12".to_owned()));
    let _ = std::fs::remove_dir_all(root);
}

#[gpui::test]
fn switching_language_relocalizes_the_device_name_placeholder(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
    cx.update(crate::test_client::init);
    let (to_host, _outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store =
        cx.new(|cx| WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx));
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    let window_state = cx.new(|_| WindowState::default());
    let mut page = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
        page = Some(view.clone());
        gpui_base::Root::new(view, window, cx)
    });
    let page = page.unwrap();
    crate::settings::assert_follows_language_switch(cx, "settings.device_name.placeholder", |cx| {
        page.read_with(cx, |page, cx| {
            let input = page.device_name_input.state.read(cx);
            input.presentation().placeholder().to_string()
        })
    });
}
