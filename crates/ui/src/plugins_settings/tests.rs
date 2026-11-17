use agent::{PluginInstallation, PluginSource};
use gpui::{TestAppContext, VisualTestContext, size};
use tcode_core::project::Project;
use tcode_core::settings::{ProviderProfile, ProviderSettings, Settings};
use tcode_protocol::{
    ClientPayload, Command, CommandResponse, EventEnvelope, HostMessage, IndexSnapshot,
    ProvidersStatus, ServerEvent, Topic, decode_client_line, encode_line,
};

use super::*;
use crate::settings_page::SettingsPage;
use crate::store::WorkspaceAttachment;

/// The project as the host names it: a path on the host, which this
/// client never reads.
const ROOT: &str = "/host/work/tcode";

struct Host {
    outgoing: async_channel::Receiver<String>,
    incoming: async_channel::Sender<String>,
    store: Entity<WorkspaceStore>,
    _pump: gpui::Task<()>,
}

impl Host {
    fn replicate(&self, catalogs: Vec<ProviderPluginCatalog>, cx: &mut VisualTestContext) {
        send(&self.incoming, Topic::Providers, providers(catalogs));
        cx.run_until_parked();
        draw(cx);
    }

    /// Play the host: acknowledge every command and return them.
    fn commands(&self, cx: &mut VisualTestContext) -> Vec<Command> {
        let mut commands = Vec::new();
        loop {
            cx.run_until_parked();
            let Ok(line) = self.outgoing.try_recv() else {
                break;
            };
            let Ok(message) = decode_client_line(&line) else {
                continue;
            };
            if let ClientPayload::Command(command) = message.payload {
                commands.push(command);
                self.incoming
                    .try_send(
                        encode_line(&HostMessage::Ack {
                            id: message.id,
                            result: Ok(CommandResponse::Unit),
                        })
                        .unwrap(),
                    )
                    .unwrap();
            }
        }
        commands
    }
}

fn providers(plugins: Vec<ProviderPluginCatalog>) -> ServerEvent {
    ServerEvent::ProvidersReplaced(ProvidersStatus {
        plugins,
        // Every native harness, as a host describes it.
        harnesses: ProviderKind::NATIVE
            .into_iter()
            .map(tcode_protocol::HarnessDescription::native)
            .collect(),
        ..Default::default()
    })
}

fn send(incoming: &async_channel::Sender<String>, topic: Topic, event: ServerEvent) {
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

/// Two Claude Code accounts, the built-in `claude` and `user`, with
/// the project last worked in.
fn settings() -> Settings {
    let mut settings = Settings {
        last_project_id: Some("tcode".into()),
        ..Default::default()
    };
    settings.profiles.insert(
        "user".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                display_name: Some("user".into()),
                ..ProviderSettings::default()
            },
        },
    );
    settings
}

fn project() -> Project {
    Project {
        id: "tcode".into(),
        name: "tcode".into(),
        root: ROOT.into(),
        quick_chat: false,
        icon_path: None,
        permission_defaults: Default::default(),
        created_at: 0,
    }
}

fn entry(id: &str, actions: Vec<PluginAction>) -> ProviderPluginEntry {
    let (name, marketplace) = id.split_once('@').unwrap();
    ProviderPluginEntry {
        id: id.into(),
        name: name.into(),
        version: Some("1.0.0".into()),
        latest_version: None,
        description: Some(format!("The {name} plugin")),
        source: PluginSource {
            marketplace: Some(marketplace.into()),
            kind: PluginSourceKind::LocalPath,
        },
        installations: Vec::new(),
        enabled: Tri::Unknown,
        declared: None,
        always_on: None,
        errors: Vec::new(),
        actions,
        diagnostics: Vec::new(),
    }
}

fn installed(mut entry: ProviderPluginEntry, scope: PluginScope, on: bool) -> ProviderPluginEntry {
    entry.installations.push(PluginInstallation {
        scope,
        location: Some(format!("/host/cache/{}", entry.name).into()),
        version: entry.version.clone(),
        scope_enabled: Some(on),
        other_project: false,
    });
    entry.enabled = Tri::from(on);
    entry
}

fn catalog(
    profile: &str,
    cwd: Option<&str>,
    entries: Vec<ProviderPluginEntry>,
) -> ProviderPluginCatalog {
    ProviderPluginCatalog {
        profile_id: profile.into(),
        context_cwd: cwd.map(PathBuf::from),
        marketplaces: vec![ProviderPluginMarketplace {
            name: "mkt".into(),
            source: "/host/mkt".into(),
            kind: PluginSourceKind::LocalPath,
            location: None,
        }],
        marketplace_actions: vec![MarketplaceAction::Add],
        entries,
        errors: Vec::new(),
        state: PluginCatalogState::Fresh,
        loading: false,
        pending: Vec::new(),
        challenges: Vec::new(),
    }
}

const USER: PluginScope = PluginScope::User;
const PROJECT: PluginScope = PluginScope::Project;

/// What the host listed for the two accounts. `alpha` is installed in
/// both, on in `claude` and off in `user`; `delta` only in `claude`,
/// from a marketplace `user` lacks; `beta` is enabled by the project
/// and only `claude` has its files; `gamma` is offered to both.
fn catalogs() -> Vec<ProviderPluginCatalog> {
    let alpha = |on: bool| {
        installed(
            entry(
                "alpha@mkt",
                vec![
                    if on {
                        PluginAction::Disable { scope: USER }
                    } else {
                        PluginAction::Enable { scope: USER }
                    },
                    PluginAction::Uninstall { scope: USER },
                ],
            ),
            USER,
            on,
        )
    };
    let gamma = || entry("gamma@mkt", vec![PluginAction::Install { scope: USER }]);
    vec![
        catalog(
            "claude",
            None,
            vec![
                alpha(true),
                installed(
                    entry("delta@other", vec![PluginAction::Disable { scope: USER }]),
                    USER,
                    true,
                ),
                gamma(),
            ],
        ),
        catalog("user", None, vec![alpha(false), gamma()]),
        catalog(
            "claude",
            Some(ROOT),
            vec![installed(
                entry(
                    "beta@mkt",
                    vec![
                        PluginAction::Disable { scope: PROJECT },
                        PluginAction::Uninstall { scope: PROJECT },
                    ],
                ),
                PROJECT,
                true,
            )],
        ),
        catalog(
            "user",
            Some(ROOT),
            vec![entry(
                "beta@mkt",
                vec![PluginAction::Install { scope: PROJECT }],
            )],
        ),
    ]
}

/// Open Settings → Plugins the way navigation does, on a client attached
/// to a remote host whose replicated state holds `settings`, one project
/// and `catalogs`.
fn open_plugins(
    settings: Settings,
    catalogs: Vec<ProviderPluginCatalog>,
    cx: &mut TestAppContext,
) -> (Host, &mut VisualTestContext) {
    cx.update(crate::test_client::init);
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    send(
        &incoming,
        Topic::Settings,
        ServerEvent::SettingsSnapshot(settings),
    );
    send(
        &incoming,
        Topic::Index,
        ServerEvent::IndexSnapshot(IndexSnapshot {
            summary: Default::default(),
            sessions: Vec::new(),
            projects: vec![project()],
        }),
    );
    send(&incoming, Topic::Providers, providers(catalogs));
    let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let pump_link = link.clone();
    let pump = cx.background_executor.spawn(async move {
        pump_link.pump().await;
    });
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            link,
            WorkspaceAttachment::Remote {
                host_id: "studio".into(),
                host_name: "Studio".into(),
            },
            None,
            None,
            cx,
        )
    });
    crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
    let host = Host {
        outgoing,
        incoming,
        store,
        _pump: pump,
    };
    let window_state = cx.new(|_| WindowState::default());
    let page_store = host.store.clone();
    let (_, cx) = cx.add_window_view(|window, cx| {
        let page = cx.new(|cx| {
            let mut page = SettingsPage::new(page_store, window_state, window, cx);
            page.open_section("plugins", cx);
            page
        });
        gpui_base::Root::new(page, window, cx)
    });
    cx.simulate_resize(size(px(1400.), px(1600.)));
    cx.run_until_parked();
    draw(cx);
    (host, cx)
}

fn draw(cx: &mut VisualTestContext) {
    for _ in 0..2 {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            _ = window.draw(cx);
        });
    }
}

fn click(selector: &'static str, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is drawn"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    draw(cx);
}

/// Accounts are columns: each account's switch shows and changes that
/// account's own scope; a cell an account's listing has nothing for draws
/// no control. The project's plugin has one switch, run through an
/// account that offers it, and the account without its files can install
/// them. Every listing and change names the host's project path.
#[gpui::test]
fn each_account_has_its_column_and_the_project_one_switch(cx: &mut TestAppContext) {
    let (host, cx) = open_plugins(settings(), catalogs(), cx);
    let listed: Vec<(String, Option<PathBuf>)> = host
        .commands(cx)
        .into_iter()
        .filter_map(|command| match command {
            Command::RefreshProviderPlugins { profile_id, cwd } => Some((profile_id, cwd)),
            _ => None,
        })
        .collect();
    for wanted in [
        ("claude", None),
        ("claude", Some(ROOT)),
        ("user", None),
        ("user", Some(ROOT)),
    ] {
        let wanted = (wanted.0.to_string(), wanted.1.map(PathBuf::from));
        assert!(listed.contains(&wanted), "{wanted:?} in {listed:?}");
    }

    for drawn in [
        "plugin-switch-claude-alpha@mkt",
        "plugin-switch-user-alpha@mkt",
        "plugin-switch-claude-delta@other",
        "plugin-cell-user-delta@other",
        "plugin-project-switch-project-beta@mkt",
        "plugin-ready-claude-beta@mkt",
        "plugin-ready-user-beta@mkt",
    ] {
        assert!(cx.debug_bounds(drawn).is_some(), "{drawn}");
    }
    for absent in [
        "plugin-switch-user-delta@other",
        "plugin-install-user-delta@other",
        // Offered but installed nowhere: it is browsed, not listed.
        "plugin-row-user-gamma@mkt",
    ] {
        assert!(cx.debug_bounds(absent).is_none(), "{absent}");
    }

    click("plugin-switch-user-alpha@mkt", cx);
    click("plugin-project-switch-project-beta@mkt", cx);
    click("plugin-ready-install-user-beta@mkt", cx);
    let changes: Vec<Command> = host
        .commands(cx)
        .into_iter()
        .filter(|command| !matches!(command, Command::RefreshProviderPlugins { .. }))
        .collect();
    assert_eq!(
        changes,
        [
            Command::SetProviderPluginEnabled {
                profile_id: "user".into(),
                entry_id: "alpha@mkt".into(),
                scope: USER,
                enabled: true,
                cwd: None,
            },
            Command::SetProviderPluginEnabled {
                profile_id: "claude".into(),
                entry_id: "beta@mkt".into(),
                scope: PROJECT,
                enabled: false,
                cwd: Some(ROOT.into()),
            },
            Command::InstallProviderPlugin {
                profile_id: "user".into(),
                entry_id: "beta@mkt".into(),
                scope: PROJECT,
                cwd: Some(ROOT.into()),
            },
        ]
    );
}

/// Installing ticks one account and shows the command it runs there;
/// confirming installs where ticked and nowhere else.
#[gpui::test]
fn an_install_runs_only_for_the_accounts_ticked(cx: &mut TestAppContext) {
    let (host, cx) = open_plugins(settings(), catalogs(), cx);
    host.commands(cx);
    click("plugins-browse", cx);
    click("plugin-install-open-gamma@mkt", cx);
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("plugin-install-command-claude").is_some());
    assert!(cx.debug_bounds("plugin-install-command-user").is_none());
    click("plugin-install-confirm", cx);
    let installs = |host: &Host, cx: &mut VisualTestContext| -> Vec<String> {
        host.commands(cx)
            .into_iter()
            .filter_map(|command| match command {
                Command::InstallProviderPlugin {
                    profile_id,
                    entry_id,
                    scope: USER,
                    cwd: None,
                } if entry_id == "gamma@mkt" => Some(profile_id),
                _ => None,
            })
            .collect()
    };
    assert_eq!(installs(&host, cx), ["claude"]);

    crate::test_frames::draw_settled(cx);
    click("plugin-install-open-gamma@mkt", cx);
    crate::test_frames::draw_settled(cx);
    click("plugin-install-account-user", cx);
    assert!(cx.debug_bounds("plugin-install-command-user").is_some());
    click("plugin-install-confirm", cx);
    assert_eq!(installs(&host, cx), ["claude", "user"]);
}

/// A marketplace removal waits on the person: the dialog lists what it
/// uninstalls, and Remove answers exactly that challenge, once.
#[gpui::test]
fn destructive_challenge_lists_uninstalls_and_accept_answers_it(cx: &mut TestAppContext) {
    let mut pending = catalogs();
    pending[0].challenges.push(PluginChallenge {
        op_id: RuntimeOperationId(7),
        profile_id: "claude".into(),
        entry_id: "mkt".into(),
        kind: PluginChallengeKind::ConfirmDestructive {
            uninstalls: vec!["alpha@mkt".into(), "beta@mkt".into()],
        },
        native_text: None,
    });
    let (host, cx) = open_plugins(settings(), pending.clone(), cx);
    assert!(
        cx.debug_bounds("plugin-challenge-uninstall-alpha@mkt")
            .is_some()
    );
    assert!(
        cx.debug_bounds("plugin-challenge-uninstall-beta@mkt")
            .is_some()
    );
    host.commands(cx);

    click("plugin-challenge-accept", cx);
    let answers: Vec<Command> = host
        .commands(cx)
        .into_iter()
        .filter(|command| matches!(command, Command::ResolvePluginChallenge { .. }))
        .collect();
    assert_eq!(
        answers,
        [Command::ResolvePluginChallenge {
            op_id: RuntimeOperationId(7),
            accept: true
        }]
    );
    // The replica still shows the challenge until the host has acted on
    // the answer; the person is not asked again meanwhile.
    host.replicate(pending, cx);
    assert!(cx.debug_bounds("plugin-challenge-accept").is_none());
}
