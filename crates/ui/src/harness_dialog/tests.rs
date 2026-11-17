use super::*;
use gpui::{Modifiers, TestAppContext, VisualTestContext, size};
use tcode_core::provider_status::ProviderAuth;
use tcode_core::settings::{ProviderProfile, ProviderSettings};
use tcode_protocol::{
    ClientPayload, Command, EventEnvelope, HostMessage, IndexSnapshot, ProviderVersionStatus,
    ProvidersStatus, ServerEvent, Topic, decode_client_line, encode_line,
};

/// A client attached to a host that holds `settings` and reports
/// `providers` and `sessions`, as the host's messages would.
struct Host {
    store: Entity<WorkspaceStore>,
    outgoing: async_channel::Receiver<String>,
    incoming: async_channel::Sender<String>,
    _pump: gpui::Task<()>,
}

impl Host {
    fn new(
        settings: Settings,
        providers: ProvidersStatus,
        sessions: Vec<tcode_core::project::SessionMeta>,
        cx: &mut TestAppContext,
    ) -> Self {
        cx.update(crate::test_client::init);
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
        let deferred = vec![
            event(Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
            event(Topic::Providers, ServerEvent::ProvidersReplaced(providers)),
            event(
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions,
                    projects: Vec::new(),
                }),
            ),
        ];
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
        cx.run_until_parked();
        cx.run_until_parked();
        Self {
            store,
            outgoing,
            incoming,
            _pump: pump,
        }
    }

    /// The settings edits the client sent, in order.
    fn sent_patches(&self) -> Vec<(String, ProfileSettingsPatch)> {
        std::iter::from_fn(|| self.outgoing.try_recv().ok())
            .filter_map(|line| match decode_client_line(&line).ok()?.payload {
                ClientPayload::Command(Command::UpdateProfileSettings { profile_id, patch }) => {
                    Some((profile_id, patch))
                }
                _ => None,
            })
            .collect()
    }

    fn open<'a>(
        &self,
        harness: HarnessId,
        cx: &'a mut TestAppContext,
    ) -> (Entity<HarnessDialog>, &'a mut VisualTestContext) {
        let store = self.store.clone();
        let (dialog, cx) =
            cx.add_window_view(|window, cx| HarnessDialog::new(store, harness, true, window, cx));
        cx.simulate_resize(size(px(900.), px(2400.)));
        draw(cx);
        (dialog, cx)
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
}

fn ready(email: Option<&str>, label: Option<&str>) -> ProviderSnapshot {
    ProviderSnapshot {
        checked_at: Some(1),
        installed: true,
        version: Some("2.1.318".into()),
        status: Some(ProviderStatusKind::Ready),
        auth: Some(ProviderAuth {
            status: AuthStatus::Authenticated,
            label: label.map(Into::into),
            email: email.map(Into::into),
        }),
        ..ProviderSnapshot::default()
    }
}

/// Claude Code with a second account, pi, Codex and an ACP agent, every
/// harness described by the host and every profile probed ready.
fn settings_and_status() -> (Settings, ProvidersStatus) {
    let mut settings = Settings::default();
    settings.profiles.insert(
        "work".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                display_name: Some("Work".into()),
                ..ProviderSettings::default()
            },
        },
    );
    settings.acp_agents.insert(
        "gemini".into(),
        tcode_core::acp::InstalledAcpAgent {
            id: "gemini".into(),
            name: "Gemini CLI".into(),
            version: "0.31.0".into(),
            icon: None,
            launch: agent::AcpLaunch::Npx {
                package: "@google/gemini-cli".into(),
                args: vec!["--experimental-acp".into()],
                env: Vec::new(),
            },
            enabled: true,
            env: Vec::new(),
            launch_args: None,
        },
    );
    let mut status = ProvidersStatus {
        harnesses: ProviderKind::NATIVE
            .into_iter()
            .map(HarnessDescription::native)
            .chain([HarnessDescription {
                acp_agent_id: Some("gemini".into()),
                launch_summary: Some("npx @google/gemini-cli --experimental-acp".into()),
                launch_args: true,
                mcp_servers: true,
                ..HarnessDescription::native(ProviderKind::Acp)
            }])
            .collect(),
        ..ProvidersStatus::default()
    };
    for id in [
        "claude",
        "work",
        "pi",
        "codex",
        "opencode",
        "native:cursor",
        "native:grok",
    ] {
        status.provider_snapshots.insert(
            id.into(),
            ready(
                (id == "claude").then_some("user@example.com"),
                (id == "claude").then_some("Claude Max Subscription"),
            ),
        );
    }
    (settings, status)
}

/// Which sections a harness's settings have comes from what the host says
/// the harness is: accounts listed where they can be added, one account's
/// sign-in and models on the overview otherwise, a switch where the
/// harness declares one, and an ACP agent with neither accounts nor
/// models.
#[gpui::test]
fn sections_follow_the_harness_description(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (settings, status) = settings_and_status();
    let host = Host::new(settings, status, Vec::new(), cx);
    assert_eq!(
        host.store
            .read_with(cx, |store, _| store.provider_profile_display_name("claude")),
        "Claude Code (user)",
        "beside a second account, the default one is named for who it is"
    );
    let shown =
        |cx: &mut VisualTestContext, selector: &'static str| cx.debug_bounds(selector).is_some();
    for (harness, present, absent) in [
        (
            HarnessId::Native(ProviderKind::ClaudeCode),
            &[
                "harness-accounts",
                "harness-account-work",
                "harness-add-account",
                "harness-add-endpoint",
                "harness-field-Binary",
                "harness-field-LaunchArgs",
                "harness-plugins",
                "harness-permission",
            ][..],
            &[
                "harness-models",
                "harness-field-ConfigDir",
                "harness-remove-agent",
            ][..],
        ),
        (
            HarnessId::Native(ProviderKind::Codex),
            &[
                "harness-accounts",
                "harness-add-account",
                "harness-plugins",
                "harness-permission",
            ][..],
            &["harness-add-endpoint", "harness-field-LaunchArgs"][..],
        ),
        (
            HarnessId::Native(ProviderKind::Pi),
            &[
                "harness-models",
                "harness-field-ConfigDir",
                "harness-toggle-trust_project_extensions",
                "harness-plugins",
            ][..],
            &["harness-accounts", "harness-permission"][..],
        ),
        (
            HarnessId::Acp("gemini".into()),
            &["harness-field-LaunchArgs", "harness-remove-agent"][..],
            &[
                "harness-accounts",
                "harness-models",
                "harness-field-Binary",
                "harness-plugins",
                "harness-permission",
            ][..],
        ),
    ] {
        let (_dialog, cx) = host.open(harness.clone(), cx);
        for selector in present {
            assert!(shown(cx, selector), "{harness:?} must show {selector}");
        }
        for selector in absent {
            assert!(!shown(cx, selector), "{harness:?} must not show {selector}");
        }
    }
}

/// Secrets the host could not read are not shown as none saved: the
/// variables say why.
#[gpui::test]
fn unreadable_secrets_are_said_beside_the_variables(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (settings, mut status) = settings_and_status();
    status.secrets_unreadable = Some("secrets.json is unreadable".into());
    let host = Host::new(settings, status, Vec::new(), cx);
    let (_dialog, cx) = host.open(HarnessId::Native(ProviderKind::Pi), cx);
    assert!(cx.debug_bounds("harness-secrets-unreadable").is_some());
}

/// The dialog lists what the host read from the harness's own files and
/// offers nothing to change there; the default permission it does offer
/// goes to the host as the harness's patch.
#[gpui::test]
fn a_harness_shows_its_own_mcp_servers_and_saves_its_default_permission(cx: &mut TestAppContext) {
    use tcode_protocol::{ClientPayload, Command, Query, QueryResponse, decode_client_line};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (settings, status) = settings_and_status();
    let host = Host::new(settings, status, Vec::new(), cx);
    let outgoing = host.outgoing.clone();
    let incoming = host.incoming.clone();
    let sent = move || -> Vec<(u64, ClientPayload)> {
        std::iter::from_fn(|| outgoing.try_recv().ok())
            .map(|line| decode_client_line(&line).unwrap())
            .map(|message| (message.id, message.payload))
            .collect()
    };
    sent();
    let (_dialog, cx) = host.open(HarnessId::Native(ProviderKind::ClaudeCode), cx);
    assert!(cx.debug_bounds("harness-native-mcp").is_none());
    let id = sent()
        .into_iter()
        .find_map(|(id, payload)| match payload {
            ClientPayload::Query(Query::NativeMcpServers { profile_id }) => {
                assert_eq!(profile_id, "claude");
                Some(id)
            }
            _ => None,
        })
        .expect("opening asks the host");
    incoming
        .try_send(
            encode_line(&HostMessage::QueryResult {
                id,
                result: Ok(QueryResponse::NativeMcpServers(vec![
                    agent::NativeMcpServer {
                        name: "linear".into(),
                        remote: true,
                        target: "https://mcp.linear.app".into(),
                        enabled: true,
                    },
                ])),
            })
            .unwrap(),
        )
        .unwrap();
    cx.run_until_parked();
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("harness-native-mcp-linear").is_some());

    let trigger = cx.debug_bounds("harness-permission").unwrap();
    cx.simulate_click(trigger.center(), Default::default());
    crate::test_frames::draw_settled(cx);
    let option = cx.debug_bounds("harness-permission-acceptEdits").unwrap();
    cx.simulate_click(option.center(), Default::default());
    cx.run_until_parked();
    let saved = sent().into_iter().any(|(_, payload)| {
        matches!(
            payload,
            ClientPayload::Command(Command::UpdateProfileSettings { profile_id, patch })
                if profile_id == "claude"
                    && patch
                        == ProfileSettingsPatch::SetDefaultPermission(Some(
                            "acceptEdits".into()
                        ))
        )
    });
    assert!(saved, "the choice reaches the host");
}

/// What is wrong with a harness is said at the top of its settings: off,
/// failing to run, older than Tcode supports, out of date; and a harness
/// whose CLI is missing shows only how to install it.
#[gpui::test]
fn states_show_as_banners_and_a_missing_cli_as_its_install(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (mut settings, mut status) = settings_and_status();
    settings.provider_mut(ProviderKind::ClaudeCode).enabled = false;
    settings.profiles.get_mut("work").unwrap().settings.enabled = false;
    status.provider_snapshots.insert(
        "pi".into(),
        ProviderSnapshot {
            checked_at: Some(1),
            installed: true,
            status: Some(ProviderStatusKind::Error),
            diagnostic: Some(ProviderProbeDiagnostic::FailedCli),
            message: Some("exit status: 1: Error: Cannot find module 'node:sqlite'".into()),
            ..ProviderSnapshot::default()
        },
    );
    status.provider_snapshots.insert(
        "native:cursor".into(),
        ProviderSnapshot {
            checked_at: Some(1),
            installed: false,
            status: Some(ProviderStatusKind::Error),
            diagnostic: Some(ProviderProbeDiagnostic::MissingCli),
            ..ProviderSnapshot::default()
        },
    );
    status.provider_snapshots.insert(
        "opencode".into(),
        ProviderSnapshot {
            version: Some("1.19.5".into()),
            supported_from: Some("2.0.6".into()),
            ..ready(None, None)
        },
    );
    status.provider_versions.insert(
        ProviderKind::Codex,
        ProviderVersionStatus {
            installed: Some("0.166.0".into()),
            latest: Some("0.167.0".into()),
            update_available: true,
            update_command: Some("npm install -g @openai/codex".into()),
            ..ProviderVersionStatus::default()
        },
    );
    let host = Host::new(settings, status, Vec::new(), cx);
    for (harness, present, absent) in [
        (
            ProviderKind::ClaudeCode,
            &["harness-banner-disabled", "harness-run"][..],
            &["harness-banner-failed", "harness-install"][..],
        ),
        (
            ProviderKind::Pi,
            &["harness-banner-failed"][..],
            &["harness-banner-disabled"][..],
        ),
        (
            ProviderKind::Codex,
            &["harness-banner-update"][..],
            &["harness-banner-disabled", "harness-banner-outdated"][..],
        ),
        (
            ProviderKind::OpenCode,
            &["harness-banner-outdated"][..],
            &["harness-banner-failed", "harness-install"][..],
        ),
        (
            ProviderKind::Cursor,
            &["harness-install"][..],
            &["harness-run", "harness-models", "harness-accounts"][..],
        ),
    ] {
        let (_dialog, cx) = host.open(HarnessId::Native(harness), cx);
        for selector in present {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{harness:?}: {selector}"
            );
        }
        for selector in absent {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "{harness:?}: no {selector}"
            );
        }
    }
}

/// A field is saved the moment it is left, with no Save to press: the
/// harness's binary as its own, an account's name as that account's.
#[gpui::test]
fn fields_are_saved_as_they_are_left(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (settings, status) = settings_and_status();
    let host = Host::new(settings, status, Vec::new(), cx);
    let (_, cx) = host.open(HarnessId::Native(ProviderKind::ClaudeCode), cx);
    host.sent_patches();

    let field = cx.debug_bounds("harness-field-Binary").unwrap();
    cx.simulate_click(field.center(), Modifiers::default());
    cx.simulate_input("/opt/claude");
    cx.simulate_keystrokes("enter");
    draw(cx);

    let row = cx.debug_bounds("harness-account-work").unwrap();
    cx.simulate_click(row.center(), Modifiers::default());
    draw(cx);
    let name = cx.debug_bounds("harness-field-Name").unwrap();
    cx.simulate_click(name.center(), Modifiers::default());
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-a"
    } else {
        "ctrl-a"
    });
    cx.simulate_input("Office");
    // Leaving the field by clicking elsewhere saves it too.
    let back = cx.debug_bounds("harness-back").unwrap();
    cx.simulate_click(back.center(), Modifiers::default());
    draw(cx);

    assert_eq!(
        host.sent_patches(),
        vec![
            (
                "claude".into(),
                ProfileSettingsPatch::SetBinaryPath(Some("/opt/claude".into()))
            ),
            (
                "work".into(),
                ProfileSettingsPatch::SetDisplayName(Some("Office".into()))
            ),
        ]
    );
}

/// Deleting an account says how many threads still run on it and which
/// account they move to.
#[gpui::test]
fn deleting_an_account_counts_the_threads_that_move(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (settings, status) = settings_and_status();
    let thread = |profile: Option<&str>| {
        let mut meta =
            tcode_core::project::SessionMeta::new(ProviderKind::ClaudeCode, "/repo".into(), None);
        meta.profile_id = profile.map(Into::into);
        meta
    };
    let sessions = vec![thread(Some("work")), thread(Some("work")), thread(None)];
    let host = Host::new(settings, status, sessions, cx);
    let (title, body) = host.store.read_with(cx, |store, _| {
        delete_confirmation(store, "work", ProviderKind::ClaudeCode, false)
    });
    assert_eq!(title, "Delete Claude Code (Work)?");
    assert_eq!(
        body,
        "Also deletes the config directory Tcode created for it, with its sessions and \
             sign-in. The 2 threads still using it switch to Claude Code (user)."
    );
}
