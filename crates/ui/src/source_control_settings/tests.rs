use super::*;
use crate::store::WorkspaceAttachment;
use gpui::{AppContext as _, TestAppContext, VisualTestContext};
use std::collections::BTreeMap;
use tcode_core::settings::{HostAccount, TokenCheck};
use tcode_protocol::{
    ClientPayload, Command, CommandResponse, EventEnvelope, HostMessage, ServerEvent,
    SettingsPatch, Topic, decode_client_line, encode_line,
};

fn status(kind: HostKind, source: Option<CredentialSource>) -> HostStatus {
    HostStatus {
        kind,
        added: false,
        token_set: matches!(source, Some(CredentialSource::Saved)),
        source,
        accounts: Vec::new(),
        env_overrides_account: false,
        order: vec![CredentialSource::Saved],
        problem: None,
        token_url: Some("https://platform.example/tokens".into()),
        access_token_url: None,
        token_scopes: vec!["api".into()],
        login: None,
    }
}

/// What a machine reports: github.com through a gh login with two accounts, a self-hosted
/// Forgejo whose saved token it refused, gitlab.com with glab installed and signed out, and an
/// Azure organization the user paused.
fn reported() -> tcode_core::settings::Settings {
    let mut settings = tcode_core::settings::Settings {
        language: Some(crate::LANGUAGE_ENGLISH.into()),
        ..Default::default()
    };
    let control = &mut settings.source_control;
    control.hosts = BTreeMap::from([
        (
            "git.example.dev".to_owned(),
            HostSettings::new(HostKind::Forgejo),
        ),
        (
            "dev.azure.com/fabrikam".to_owned(),
            HostSettings {
                enabled: false,
                ..HostSettings::new(HostKind::Azure)
            },
        ),
    ]);
    let mut github = status(
        HostKind::Github,
        Some(CredentialSource::Cli { tool: "gh".into() }),
    );
    github.accounts = vec!["lin-dev".into(), "lin-work".into()];
    github.login = Some("lin-dev".into());
    let mut forgejo = status(HostKind::Forgejo, Some(CredentialSource::Saved));
    forgejo.added = true;
    forgejo.problem = Some(HostProblem::Rejected {
        status: 401,
        at: 1_700_000_000,
    });
    let mut gitlab = status(HostKind::Gitlab, None);
    gitlab.problem = Some(HostProblem::NotSignedIn {
        tool: "glab".into(),
        command: Some("glab auth login --hostname gitlab.com".into()),
    });
    let mut azure = status(HostKind::Azure, Some(CredentialSource::Saved));
    azure.added = true;
    control.status = BTreeMap::from([
        ("github.com".to_owned(), github),
        ("git.example.dev".to_owned(), forgejo),
        ("gitlab.com".to_owned(), gitlab),
        ("dev.azure.com/fabrikam".to_owned(), azure),
    ]);
    settings
}

struct Remote {
    store: Entity<WorkspaceStore>,
    outgoing: async_channel::Receiver<String>,
    incoming: async_channel::Sender<String>,
    _pump: gpui::Task<()>,
}

impl Remote {
    /// A client attached to another machine over the link, showing what the machine reports.
    fn attach(settings: tcode_core::settings::Settings, cx: &mut TestAppContext) -> Self {
        cx.update(crate::theme::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::SettingsSnapshot(settings),
                }))
                .unwrap(),
            )
            .unwrap();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump = link.clone();
        let executor = cx.background_executor.clone();
        let pump = cx.background_executor.spawn(async move {
            pump.pump_with_timer(|delay| executor.timer(delay)).await;
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
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        Self {
            store,
            outgoing,
            incoming,
            _pump: pump,
        }
    }

    /// The commands the client sent since last asked, with their ids.
    fn commands(&self) -> Vec<(u64, Command)> {
        std::iter::from_fn(|| self.outgoing.try_recv().ok())
            .filter_map(|line| {
                let message = decode_client_line(&line).ok()?;
                match message.payload {
                    ClientPayload::Command(command) => Some((message.id, command)),
                    _ => None,
                }
            })
            .collect()
    }

    fn answer(&self, id: u64, response: CommandResponse) {
        self.incoming
            .try_send(
                encode_line(&HostMessage::Ack {
                    id,
                    result: Ok(response),
                })
                .unwrap(),
            )
            .unwrap();
    }
}

fn open_panel<'a>(
    remote: &Remote,
    compact: bool,
    cx: &'a mut TestAppContext,
) -> &'a mut VisualTestContext {
    let window_state =
        cx.new(|_| crate::window_state::WindowState::new(false).with_compact(compact));
    let store = remote.store.clone();
    let (_, cx) = cx.add_window_view(|window, cx| {
        let panel = cx.new(|cx| {
            let mut panel = SourceControlPanel::new(store, window_state, cx);
            panel.show(cx);
            panel
        });
        gpui_base::Root::new(panel, window, cx)
    });
    draw(remote, cx);
    cx
}

/// Lets what the machine sent land and what it started play out, as frames do in the app.
fn draw(remote: &Remote, cx: &mut VisualTestContext) {
    cx.run_until_parked();
    remote
        .store
        .update(cx, |store, cx| store.drain_host_events_for_test(cx));
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.refresh();
        _ = window.draw(cx);
    });
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(400));
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        _ = window.draw(cx);
    });
}

/// Whether the element `selector` names was drawn; the test context keeps selectors for good.
fn drawn(cx: &mut VisualTestContext, selector: String) -> bool {
    cx.debug_bounds(Box::leak(selector.into_boxed_str()))
        .is_some()
}

fn click(selector: &'static str, remote: &Remote, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is drawn"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    draw(remote, cx);
}

/// Each platform's row says where it stands from what the machine reports, and offers one
/// action: Connect where nothing is in use, Manage otherwise.
#[gpui::test]
fn rows_show_each_platforms_standing_and_one_action(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let remote = Remote::attach(reported(), cx);
    let cx = open_panel(&remote, false, cx);
    assert!(
        remote
            .commands()
            .iter()
            .any(|(_, command)| *command == Command::RefreshHostCredentials),
        "opening the page asks the machine to report afresh"
    );
    for (host, standing, action) in [
        ("github.com", "connected", "manage"),
        ("git.example.dev", "rejected", "manage"),
        ("gitlab.com", "not-signed-in", "connect"),
        ("dev.azure.com/fabrikam", "paused", "manage"),
    ] {
        assert!(
            drawn(cx, format!("source-control-standing-{host}-{standing}")),
            "{host} reads {standing}"
        );
        assert!(
            drawn(cx, format!("source-control-{action}-{host}")),
            "{host} offers {action}"
        );
        let other = if action == "manage" {
            "connect"
        } else {
            "manage"
        };
        assert!(!drawn(cx, format!("source-control-{other}-{host}")));
    }
    assert!(cx.debug_bounds("source-control-add-host").is_some());
}

/// Connecting sends the token to the machine to verify; a refusal and an unreachable platform
/// leave it unsaved and say why, Save anyway saves it as it is, and an accepted token shows the
/// account it signs in as.
#[gpui::test]
fn connecting_verifies_the_token_before_the_machine_saves_it(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let remote = Remote::attach(reported(), cx);
    let cx = open_panel(&remote, false, cx);
    remote.commands();
    click("source-control-connect-gitlab.com", &remote, cx);
    assert!(
        cx.debug_bounds("source-control-command").is_some(),
        "the glab sign-in is offered beside the token"
    );
    cx.simulate_input("glpat-typed");
    draw(&remote, cx);
    let verify = |remote: &Remote, cx: &mut VisualTestContext, answer: TokenCheck| {
        click("source-control-verify", remote, cx);
        let sent = remote.commands();
        let [(id, command)] = sent.as_slice() else {
            panic!("one command per check: {sent:?}");
        };
        assert_eq!(
            *command,
            Command::VerifyHostToken {
                host: "gitlab.com".into(),
                token: "glpat-typed".into(),
                email: None,
            }
        );
        remote.answer(*id, CommandResponse::TokenCheck(answer));
        draw(remote, cx);
    };
    verify(&remote, cx, TokenCheck::Rejected { status: 401 });
    assert!(cx.debug_bounds("source-control-check-rejected").is_some());
    assert!(cx.debug_bounds("source-control-save-anyway").is_none());
    verify(&remote, cx, TokenCheck::Unreachable { status: None });
    assert!(
        cx.debug_bounds("source-control-check-unreachable")
            .is_some()
    );
    click("source-control-save-anyway", &remote, cx);
    assert_eq!(
        remote
            .commands()
            .into_iter()
            .map(|(_, command)| command)
            .collect::<Vec<_>>(),
        [Command::SetHostToken {
            host: "gitlab.com".into(),
            token: Some("glpat-typed".into()),
            email: None,
        }]
    );
    assert!(
        cx.debug_bounds("source-control-verify").is_none(),
        "saving closes the dialog"
    );

    click("source-control-connect-gitlab.com", &remote, cx);
    cx.simulate_input("glpat-typed");
    draw(&remote, cx);
    verify(
        &remote,
        cx,
        TokenCheck::Accepted {
            account: Some(HostAccount {
                login: "lin".into(),
                name: Some("Lin Chen".into()),
            }),
        },
    );
    assert!(cx.debug_bounds("source-control-connected").is_some());
    assert!(cx.debug_bounds("source-control-verify").is_none());
    assert!(
        remote.commands().is_empty(),
        "the machine saved it; the client sends nothing more"
    );
}

/// Managing a gh login chooses among its accounts, and a refused saved token can be
/// disconnected after confirming.
#[gpui::test]
fn managing_chooses_an_account_and_disconnects_after_confirming(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let remote = Remote::attach(reported(), cx);
    let cx = open_panel(&remote, false, cx);
    remote.commands();
    click("source-control-manage-github.com", &remote, cx);
    assert!(
        cx.debug_bounds("source-control-disconnect").is_none(),
        "a gh login is not Tcode's to disconnect"
    );
    click("source-control-account-lin-work", &remote, cx);
    assert!(remote.commands().iter().any(|(_, command)| matches!(
        command,
        Command::PatchSettings {
            patch: SettingsPatch::SourceControlHost { host, account: Some(Some(account)), .. }
        } if host == "github.com" && account == "lin-work"
    )));
    click("source-control-done", &remote, cx);

    click("source-control-manage-git.example.dev", &remote, cx);
    assert!(cx.debug_bounds("source-control-problem-rejected").is_some());
    click("source-control-disconnect", &remote, cx);
    assert!(
        remote.commands().is_empty(),
        "nothing is deleted before confirming"
    );
    cx.update(|window, cx| {
        window.dispatch_action(
            Box::new(gpui_base::actions::Confirm { secondary: false }),
            cx,
        )
    });
    draw(&remote, cx);
    assert_eq!(
        remote
            .commands()
            .into_iter()
            .map(|(_, command)| command)
            .collect::<Vec<_>>(),
        [Command::SetHostToken {
            host: "git.example.dev".into(),
            token: None,
            email: None,
        }]
    );
}

/// Adding a platform keeps the address rules, then goes straight on to connecting it.
#[gpui::test]
fn adding_a_platform_goes_on_to_connect_it(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    let remote = Remote::attach(reported(), cx);
    let cx = open_panel(&remote, false, cx);
    remote.commands();
    click("source-control-add-host", &remote, cx);
    click("source-control-kind-forgejo", &remote, cx);
    cx.simulate_keystrokes("cmd-a backspace");
    cx.simulate_input("git.example.dev");
    click("source-control-add-confirm", &remote, cx);
    assert!(
        remote.commands().is_empty(),
        "an address already listed is not added"
    );
    click("source-control-address", &remote, cx);
    cx.simulate_keystrokes("cmd-a backspace");
    cx.simulate_input("git.example.org");
    click("source-control-add-confirm", &remote, cx);
    assert_eq!(
        remote
            .commands()
            .into_iter()
            .map(|(_, command)| command)
            .collect::<Vec<_>>(),
        [Command::PatchSettings {
            patch: SettingsPatch::SourceControlHost {
                host: "git.example.org".into(),
                kind: HostKind::Forgejo,
                enabled: Some(true),
                account: None,
            }
        }]
    );
    assert!(
        cx.debug_bounds("source-control-verify").is_some(),
        "the dialog goes on to connect the platform"
    );
}
