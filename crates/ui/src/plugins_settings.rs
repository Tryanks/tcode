//! Settings → Plugins: one page per Harness whose plugins Tcode manages.
//! A Harness keeps plugins in each account's own configuration directory,
//! so the page lists plugins as rows and accounts as columns, one switch per
//! account; a project's own scopes are files every account shares, so a
//! project's plugins get one switch and a line on which accounts have them.
//! Every action, scope and confirmation comes from the catalogs the host
//! listed and the Harness's capabilities; a control the host did not offer is
//! never drawn.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use agent::{
    MarketplaceAction, PluginAction, PluginActionKind, PluginOp, PluginScope, PluginSourceKind,
    ProviderKind, ProviderPluginEntry, ProviderPluginMarketplace, Tri, UnmanagedPluginsKind,
};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, point,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_protocol::{
    PluginCatalogState, PluginChallenge, PluginChallengeKind, PluginStaleReason,
    ProviderPluginCatalog, RuntimeOperationId,
};

use crate::icon::{Icon, IconName};
use crate::material;
use crate::overlay::{DialogActions, OverlayExt as _};
use crate::scroll::ScrollableElement as _;
use crate::sizing::Sizable as _;
use crate::store::{StoreChange, TopicKind, WorkspaceStore};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::checkbox::Checkbox;
use crate::widgets::choice::{Choice, ChoiceGroup};
use crate::widgets::input::{Input, InputState};
use crate::widgets::menu::DropdownMenu as _;
use crate::widgets::switch::Switch;
use crate::widgets::tabs::{TabBar, TabItem};
use crate::widgets::tooltip::{Tooltip, TooltipTrigger as _};
use crate::widgets::{Collapsible, Popover, Spinner};
use crate::window_state::WindowState;

/// More accounts than this get one column and a switcher between them.
const MAX_COLUMNS: usize = 4;

/// The width of one account's column.
const CELL_WIDTH: f32 = 120.;

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// The window width below which a plugin's accounts stack under it.
const STACKED_BELOW: f32 = 1000.;

/// The width of a row's trailing menu.
const MENU_WIDTH: f32 = 28.;

/// Which page of Settings → Plugins shows. A compact window starts at the
/// list of Harnesses; a wide one at the Harness picked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Overview,
    Harness,
    Browse,
}

/// The install a person is composing for one plugin.
struct InstallDraft {
    entry_id: String,
    accounts: BTreeSet<String>,
    scope: PluginScope,
}

/// What the source dialog adds.
#[derive(Clone, Copy, PartialEq)]
enum AddedSource {
    Marketplace,
    Package,
}

/// Run an action the host offered, chosen from a row's menu.
#[derive(Action, Clone, PartialEq, serde::Deserialize)]
#[action(namespace = plugins_settings, no_json)]
struct RunPluginAction {
    profile_id: String,
    entry_id: String,
    action: PluginAction,
    cwd: Option<PathBuf>,
}

#[derive(Action, Clone, PartialEq, serde::Deserialize)]
#[action(namespace = plugins_settings, no_json)]
struct ChooseProject {
    project_id: String,
}

#[derive(Action, Clone, PartialEq, serde::Deserialize)]
#[action(namespace = plugins_settings, no_json)]
struct RemoveMarketplace {
    profile_id: String,
    name: String,
}

pub struct PluginsSettingsPanel {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// The settings page's content scroll, returned to the top when the page
    /// changes.
    page_scroll: ScrollHandle,
    /// Whether the page is on screen. Entering it lists every account again,
    /// and challenges are only presented while it shows.
    shown: bool,
    harness: Option<ProviderKind>,
    page: Page,
    /// The project whose scopes the page shows; until one is picked, the
    /// open thread's.
    project: Option<String>,
    /// `(profile id, directory)` listings asked for since the page showed.
    listed: HashSet<(String, Option<PathBuf>)>,
    /// Plugin ids whose details are open.
    expanded: HashSet<String>,
    search: Entity<InputState>,
    marketplace_filter: Option<String>,
    install: Option<InstallDraft>,
    /// The account shown when there are too many for a column each.
    column: Option<String>,
    /// Whether the window is laid out for touch: larger text, a page per
    /// level and sheets instead of popovers.
    touch: bool,
    marketplace_source: Entity<InputState>,
    marketplace_accounts: BTreeSet<String>,
    /// The package being added goes in the open project, not the account.
    package_in_project: bool,
    /// The challenge whose dialog is open.
    challenge: Option<RuntimeOperationId>,
    /// Challenges this client already answered; the replica keeps showing
    /// them until the host's answer arrives.
    answered: HashSet<RuntimeOperationId>,
    /// The page's size-class motion: the window draws it under another
    /// parent when it turns compact or wide.
    motion: crate::motion::owned::Owned,
    _subscriptions: Vec<Subscription>,
}

/// One account of the Harness shown, with what the host listed for it.
#[derive(Clone)]
struct Account {
    id: String,
    name: String,
    color: Hsla,
    binary: Option<PathBuf>,
    /// Listed from outside any project: its own user scope.
    user: Option<ProviderPluginCatalog>,
    /// Listed from the project shown: the scopes it shares.
    project: Option<ProviderPluginCatalog>,
}

impl Account {
    fn entry(&self, project: bool, id: &str) -> Option<&ProviderPluginEntry> {
        let catalog = if project { &self.project } else { &self.user };
        catalog
            .as_ref()?
            .entries
            .iter()
            .find(|entry| entry.id == id)
    }

    fn busy(&self) -> bool {
        [&self.user, &self.project]
            .into_iter()
            .flatten()
            .any(|catalog| !catalog.pending.is_empty())
    }

    fn pending(&self, id: &str) -> bool {
        [&self.user, &self.project]
            .into_iter()
            .flatten()
            .any(|catalog| catalog.pending.iter().any(|pending| pending == id))
    }

    fn loading(&self) -> bool {
        [&self.user, &self.project]
            .into_iter()
            .flatten()
            .any(|catalog| catalog.loading)
    }

    /// Whether the user stopped one of its listings before it finished.
    fn stopped(&self) -> bool {
        [&self.user, &self.project]
            .into_iter()
            .flatten()
            .any(stopped)
    }

    fn failure(&self) -> Option<String> {
        [&self.user, &self.project]
            .into_iter()
            .flatten()
            .find_map(|catalog| match &catalog.state {
                PluginCatalogState::Error { message } => Some(message.clone()),
                _ => None,
            })
    }
}

/// One plugin as the page draws it: what describes it, from whichever
/// account listed it first, and each account's own entry.
struct Row {
    entry: ProviderPluginEntry,
    cells: Vec<Option<ProviderPluginEntry>>,
}

impl Row {
    fn versions_differ(&self, scope: PluginScope) -> bool {
        let versions: BTreeSet<&str> = self
            .cells
            .iter()
            .flatten()
            .filter_map(|entry| installation(entry, scope)?.version.as_deref())
            .collect();
        versions.len() > 1
    }
}

/// The installation of `entry` in `scope` that the listing's own context
/// holds.
fn installation(
    entry: &ProviderPluginEntry,
    scope: PluginScope,
) -> Option<&agent::PluginInstallation> {
    entry
        .installations
        .iter()
        .find(|installation| installation.scope == scope && !installation.other_project)
}

fn offers(entry: &ProviderPluginEntry, action: PluginAction) -> bool {
    entry.actions.contains(&action)
}

/// The value a scope's switch shows: the scope's own where the CLI reports
/// it, else the effective one.
fn scope_enabled(entry: &ProviderPluginEntry, scope: PluginScope) -> bool {
    installation(entry, scope)
        .and_then(|installation| installation.scope_enabled)
        .unwrap_or(entry.enabled == Tri::Yes)
}

/// Plugins in `scope` across `accounts`, keyed by id in first-listed order.
fn rows(accounts: &[Account], project: bool, scope: PluginScope) -> Vec<Row> {
    let mut ids: Vec<String> = Vec::new();
    for account in accounts {
        let catalog = if project {
            &account.project
        } else {
            &account.user
        };
        for entry in catalog.iter().flat_map(|catalog| &catalog.entries) {
            if installation(entry, scope).is_some() && !ids.contains(&entry.id) {
                ids.push(entry.id.clone());
            }
        }
    }
    ids.into_iter()
        .filter_map(|id| {
            let cells: Vec<Option<ProviderPluginEntry>> = accounts
                .iter()
                .map(|account| account.entry(project, &id).cloned())
                .collect();
            let entry = cells
                .iter()
                .flatten()
                .find(|entry| installation(entry, scope).is_some())
                .cloned()?;
            Some(Row { entry, cells })
        })
        .collect()
}

impl PluginsSettingsPanel {
    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        page_scroll: ScrollHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx));
        let marketplace_source = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &store,
                window,
                |this, _, change: &StoreChange, window, cx| match change.topic {
                    TopicKind::Providers | TopicKind::Settings | TopicKind::Index => {
                        if this.shown {
                            this.list_missing(cx);
                            this.sync_challenge(window, cx);
                        }
                        cx.notify();
                    }
                    TopicKind::SessionStatus => cx.notify(),
                    _ => {}
                },
            ),
            cx.observe(&window_state, |_, _, cx| cx.notify()),
            cx.observe(&search, |_, _, cx| cx.notify()),
        ];
        Self {
            store,
            window_state,
            page_scroll,
            shown: false,
            harness: None,
            page: Page::Overview,
            project: None,
            listed: HashSet::new(),
            expanded: HashSet::new(),
            search,
            marketplace_filter: None,
            install: None,
            column: None,
            touch: false,
            marketplace_source,
            marketplace_accounts: BTreeSet::new(),
            package_in_project: false,
            challenge: None,
            answered: HashSet::new(),
            motion: Default::default(),
            _subscriptions: subscriptions,
        }
    }

    /// The page left the screen; entering it again lists the catalogs anew.
    pub fn hide(&mut self) {
        self.shown = false;
        self.listed.clear();
    }

    /// Open the Harness of `profile_id`'s plugins.
    pub fn focus_profile(&mut self, profile_id: String, cx: &mut Context<Self>) {
        self.harness = self.store.read(cx).provider_profile_kind(&profile_id);
        self.page = Page::Harness;
        self.column = Some(profile_id);
        self.page_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn compact(&self, cx: &App) -> bool {
        self.window_state.read(cx).compact
    }

    /// Harnesses whose plugins Tcode manages, with an enabled account.
    fn harnesses(&self, cx: &App) -> Vec<ProviderKind> {
        let store = self.store.read(cx);
        let profiles = store.enabled_profiles();
        ProviderKind::NATIVE
            .into_iter()
            .filter(|kind| plugins_of(store, *kind).manages())
            .filter(|kind| profiles.iter().any(|profile| profile.kind == *kind))
            .collect()
    }

    fn harness_shown(&self, cx: &App) -> Option<ProviderKind> {
        let harnesses = self.harnesses(cx);
        self.harness
            .filter(|kind| harnesses.contains(kind))
            .or(harnesses.first().copied())
    }

    /// The project whose scopes show: the one picked, else the open
    /// thread's, else the one last worked in.
    fn project(&self, cx: &App) -> Option<tcode_core::project::Project> {
        let store = self.store.read(cx);
        let projects = store.projects();
        let picked = self
            .project
            .clone()
            .or_else(|| {
                let thread = store.session_of(crate::shell::current_thread(cx)?)?;
                store.thread_meta(&thread)?.project_id.clone()
            })
            .or_else(|| store.last_project_id().map(str::to_string))?;
        projects.into_iter().find(|project| project.id == picked)
    }

    fn project_root(&self, cx: &App) -> Option<PathBuf> {
        self.project(cx).map(|project| project.root)
    }

    fn accounts(&self, kind: ProviderKind, cx: &App) -> Vec<Account> {
        let store = self.store.read(cx);
        let root = self.project_root(cx);
        store
            .enabled_profiles()
            .into_iter()
            .filter(|profile| profile.kind == kind)
            .map(|profile| Account {
                name: store.provider_profile_display_name(&profile.id),
                color: gpui::rgb(store.provider_profile_color(&profile.id)).into(),
                binary: profile.settings.binary_path.clone(),
                user: store.provider_plugin_catalog(&profile.id, None).cloned(),
                project: root.as_deref().and_then(|root| {
                    store
                        .provider_plugin_catalog(&profile.id, Some(root))
                        .cloned()
                }),
                id: profile.id,
            })
            .collect()
    }

    /// The listings each account needs: its own scope, and the project's
    /// where the Harness has project scopes.
    fn wanted(&self, cx: &App) -> Vec<(String, Option<PathBuf>)> {
        let root = self.project_root(cx);
        let store = self.store.read(cx);
        let mut wanted = Vec::new();
        for profile in store.enabled_profiles() {
            let management = plugins_of(store, profile.kind);
            if !management.manages() {
                continue;
            }
            wanted.push((profile.id.clone(), None));
            if management.scopes.contains(&PluginScope::Project)
                && let Some(root) = &root
            {
                wanted.push((profile.id.clone(), Some(root.clone())));
            }
        }
        wanted
    }

    fn list_missing(&mut self, cx: &mut App) {
        for (profile_id, cwd) in self.wanted(cx) {
            if self.listed.insert((profile_id.clone(), cwd.clone())) {
                self.store.update(cx, |store, _| {
                    store.refresh_provider_plugins(profile_id, cwd)
                });
            }
        }
    }

    fn relist(&mut self, kind: ProviderKind, cx: &mut App) {
        let store = self.store.read(cx);
        let ours: Vec<(String, Option<PathBuf>)> = self
            .wanted(cx)
            .into_iter()
            .filter(|(profile_id, _)| store.provider_profile_kind(profile_id) == Some(kind))
            .collect();
        for (profile_id, cwd) in ours {
            self.listed.insert((profile_id.clone(), cwd.clone()));
            self.store.update(cx, |store, _| {
                store.refresh_provider_plugins(profile_id, cwd)
            });
        }
    }

    fn run(
        &self,
        account: &str,
        entry: &str,
        action: PluginAction,
        cwd: Option<PathBuf>,
        cx: &mut App,
    ) {
        let (account, entry) = (account.to_string(), entry.to_string());
        self.store.update(cx, |store, _| {
            store.run_provider_plugin_action(account, entry, action, cwd)
        });
    }

    fn pending_challenges(&self, cx: &App) -> Vec<PluginChallenge> {
        self.store
            .read(cx)
            .provider_plugin_catalogs()
            .iter()
            .flat_map(|catalog| catalog.challenges.iter().cloned())
            .collect()
    }

    /// Keep one dialog open for the first unanswered challenge in the
    /// replica, and close it once the host no longer waits on it.
    fn sync_challenge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let challenges = self.pending_challenges(cx);
        if let Some(open) = self.challenge {
            if challenges.iter().any(|challenge| challenge.op_id == open) {
                return;
            }
            self.challenge = None;
            window.close_dialog(cx);
        }
        if !self.shown {
            return;
        }
        let Some(next) = challenges
            .into_iter()
            .find(|challenge| !self.answered.contains(&challenge.op_id))
        else {
            return;
        };
        self.challenge = Some(next.op_id);
        self.open_challenge(next, window, cx);
    }

    fn open_challenge(
        &self,
        challenge: PluginChallenge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let panel = cx.entity().downgrade();
        let op_id = challenge.op_id;
        let answer = Rc::new(move |accept: bool, cx: &mut App| {
            store.update(cx, |store, _| store.resolve_plugin_challenge(op_id, accept));
            _ = panel.update(cx, |panel: &mut Self, _| {
                panel.answered.insert(op_id);
                if panel.challenge == Some(op_id) {
                    panel.challenge = None;
                }
            });
        });
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (title, accept_label, decline_label, danger) = match &challenge.kind {
                PluginChallengeKind::AcceptCommand { .. } => (
                    crate::tr!(
                        "providers.plugins.challenge.accept_command_title",
                        name = challenge.entry_id
                    ),
                    crate::tr!("providers.plugins.challenge.accept"),
                    crate::tr!("providers.plugins.challenge.decline"),
                    false,
                ),
                PluginChallengeKind::ConfirmDestructive { .. } => (
                    crate::tr!(
                        "providers.plugins.challenge.destructive_title",
                        name = challenge.entry_id
                    ),
                    crate::tr!("providers.plugins.challenge.remove"),
                    crate::tr!("providers.plugins.challenge.cancel"),
                    true,
                ),
            };
            let decline = answer.clone();
            let accept = answer.clone();
            let escape = answer.clone();
            let accept_button = Button::new("plugin-challenge-accept")
                .debug_selector(|| "plugin-challenge-accept".into())
                .label(accept_label.into_owned())
                .on_click(move |_, window, cx| {
                    accept(true, cx);
                    window.close_dialog(cx);
                });
            alert
                .wide()
                .title(SharedString::from(title.into_owned()))
                .description(challenge_body(&challenge, cx))
                .on_cancel(move |_, _, cx| {
                    escape(false, cx);
                    true
                })
                .footer(
                    DialogActions::new()
                        .flex_wrap()
                        .child(
                            Button::new("plugin-challenge-decline")
                                .debug_selector(|| "plugin-challenge-decline".into())
                                .label(decline_label.into_owned())
                                .on_click(move |_, window, cx| {
                                    decline(false, cx);
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(if danger {
                            accept_button.danger()
                        } else {
                            accept_button.primary()
                        }),
                )
        });
    }

    /// Ask for a marketplace source and the accounts to add it to.
    /// A dialog taking a source and the accounts it goes to. `project` is
    /// where a package can go instead of its accounts.
    fn open_add_source(
        &mut self,
        added: AddedSource,
        accounts: Vec<Account>,
        project: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let first = self.default_account(&accounts, cx);
        self.marketplace_accounts = first.into_iter().collect();
        self.package_in_project = false;
        let (title, help, placeholder) = match added {
            AddedSource::Marketplace => (
                crate::tr!("providers.plugins.add_marketplace_title"),
                crate::tr!("providers.plugins.marketplace_source_help"),
                crate::tr!("providers.plugins.marketplace_source_placeholder"),
            ),
            AddedSource::Package => (
                crate::tr!("providers.plugins.add_package_title"),
                crate::tr!("providers.plugins.package_source_help"),
                crate::tr!("providers.plugins.package_source_placeholder"),
            ),
        };
        let (title, help) = (title.into_owned(), help.into_owned());
        self.marketplace_source.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.set_placeholder(placeholder, window, cx);
        });
        let panel = cx.entity();
        let input = self.marketplace_source.clone();
        let store = self.store.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let panel = panel.clone();
            let field = input.clone();
            let submit_input = input.clone();
            let submit_panel = panel.clone();
            let accounts = accounts.clone();
            let store = store.clone();
            let help = help.clone();
            let project = project.clone();
            let in_project = project.clone();
            let submit_project = project.clone();
            dialog
                .wide()
                .title(title.clone())
                .content(move |content, _, cx| {
                    let chosen = panel.read(cx).marketplace_accounts.clone();
                    let here = panel.read(cx).package_in_project;
                    let toggle = panel.clone();
                    content.child(
                        v_flex()
                            .w_full()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_caption(cx)
                                    .child(crate::tr!("providers.plugins.marketplace_source")),
                            )
                            .child(Input::new(&field))
                            .child(div().text_secondary(false, cx).child(help.clone()))
                            .when(in_project.is_some(), |column| {
                                column.child(
                                    Checkbox::new("plugin-package-in-project")
                                        .label(crate::tr!("providers.plugins.package_in_project"))
                                        .checked(here)
                                        .on_click(move |checked, _, cx| {
                                            let checked = *checked;
                                            toggle.update(cx, |panel, cx| {
                                                panel.package_in_project = checked;
                                                cx.notify();
                                            });
                                        }),
                                )
                            })
                            .child(
                                div()
                                    .pt_2()
                                    .text_caption(cx)
                                    .child(crate::tr!("providers.plugins.add_to_accounts")),
                            )
                            .children(accounts.iter().map(|account| {
                                let id = account.id.clone();
                                let panel = panel.clone();
                                let selector = format!("plugin-marketplace-account-{id}");
                                div().debug_selector(move || selector.clone()).child(
                                    Checkbox::new(SharedString::from(format!(
                                        "plugin-marketplace-account-{id}"
                                    )))
                                    .label(account.name.clone())
                                    .checked(chosen.contains(&id))
                                    .on_click(
                                        move |checked, _, cx| {
                                            let id = id.clone();
                                            let checked = *checked;
                                            panel.update(cx, |panel, cx| {
                                                if checked {
                                                    panel.marketplace_accounts.insert(id);
                                                } else {
                                                    panel.marketplace_accounts.remove(&id);
                                                }
                                                cx.notify();
                                            });
                                        },
                                    ),
                                )
                            })),
                    )
                })
                .footer(
                    DialogActions::new()
                        .child(
                            Button::new("plugin-marketplace-cancel")
                                .ghost()
                                .label(crate::tr!("settings.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("plugin-marketplace-submit")
                                .debug_selector(|| "plugin-marketplace-submit".into())
                                .primary()
                                .label(crate::tr!("providers.plugins.add").into_owned())
                                .on_click(move |_, window, cx| {
                                    let source = submit_input.read(cx).value().trim().to_string();
                                    let chosen = submit_panel.read(cx).marketplace_accounts.clone();
                                    if source.is_empty() || chosen.is_empty() {
                                        return;
                                    }
                                    let here = submit_panel.read(cx).package_in_project;
                                    let (scope, cwd) = match (here, submit_project.clone()) {
                                        (true, Some(root)) => (PluginScope::Project, Some(root)),
                                        _ => (PluginScope::User, None),
                                    };
                                    store.update(cx, |store, _| {
                                        for profile_id in chosen {
                                            match added {
                                                AddedSource::Marketplace => store
                                                    .add_provider_marketplace(
                                                        profile_id,
                                                        source.clone(),
                                                        None,
                                                    ),
                                                AddedSource::Package => store
                                                    .run_provider_plugin_action(
                                                        profile_id,
                                                        source.clone(),
                                                        PluginAction::Install { scope },
                                                        cwd.clone(),
                                                    ),
                                            }
                                        }
                                    });
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    /// The account an install or a marketplace starts ticked for: the open
    /// thread's when it is one of these, else the first.
    fn default_account(&self, accounts: &[Account], cx: &App) -> Option<String> {
        let store = self.store.read(cx);
        let thread_profile = crate::shell::current_thread(cx)
            .and_then(|thread| store.session_of(thread))
            .and_then(|thread| store.thread_meta(&thread).cloned())
            .map(|meta| {
                meta.profile_id.unwrap_or_else(|| {
                    tcode_core::settings::Settings::builtin_profile_id(meta.provider).into()
                })
            });
        thread_profile
            .filter(|id| accounts.iter().any(|account| account.id == *id))
            .or_else(|| accounts.first().map(|account| account.id.clone()))
    }

    /// Start composing an install of `entry_id`, ticking one account.
    fn begin_install(&mut self, entry_id: String, accounts: &[Account], cx: &mut Context<Self>) {
        let installable: Vec<Account> = accounts
            .iter()
            .filter(|account| !install_scopes(account, &entry_id).is_empty())
            .cloned()
            .collect();
        let first = self
            .default_account(&installable, cx)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let scope = first
            .iter()
            .filter_map(|id| installable.iter().find(|account| account.id == *id))
            .flat_map(|account| install_scopes(account, &entry_id))
            .next()
            .unwrap_or(PluginScope::User);
        self.install = Some(InstallDraft {
            entry_id,
            accounts: first,
            scope,
        });
        cx.notify();
    }

    fn confirm_install(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.install.take() else {
            return;
        };
        let cwd = match draft.scope {
            PluginScope::User => None,
            _ => self.project_root(cx),
        };
        for account in &draft.accounts {
            self.run(
                account,
                &draft.entry_id,
                PluginAction::Install { scope: draft.scope },
                cwd.clone(),
                cx,
            );
        }
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        self.page = match self.page {
            Page::Browse => Page::Harness,
            Page::Harness | Page::Overview => Page::Overview,
        };
        self.install = None;
        self.page_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn open(&mut self, page: Page, kind: ProviderKind, cx: &mut Context<Self>) {
        self.harness = Some(kind);
        self.page = page;
        self.install = None;
        self.page_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }
}

/// The scopes `account` can install `entry_id` in: its own from its own
/// listing, the project's from the project's.
fn install_scopes(account: &Account, entry_id: &str) -> Vec<PluginScope> {
    let mut scopes = Vec::new();
    for (project, catalog) in [(false, &account.user), (true, &account.project)] {
        let Some(entry) = catalog
            .as_ref()
            .and_then(|catalog| catalog.entries.iter().find(|entry| entry.id == entry_id))
        else {
            continue;
        };
        for action in &entry.actions {
            if let PluginAction::Install { scope } = action
                && (*scope == PluginScope::User) != project
                && !scopes.contains(scope)
            {
                scopes.push(*scope);
            }
        }
    }
    scopes
}

impl Render for PluginsSettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.shown && self.store.read(cx).settings_hydrated() {
            self.shown = true;
            self.list_missing(cx);
            cx.defer_in(window, |this, window, cx| this.sync_challenge(window, cx));
        }
        let compact = self.compact(cx);
        self.touch = compact;
        // Below this width the columns do not fit beside a plugin's name, so
        // each plugin stacks its accounts under it as on a phone.
        let stacked = compact || window.viewport_size().width < px(STACKED_BELOW);
        let kind = self.harness_shown(cx);
        let page = match (self.page, kind) {
            (_, None) => Page::Overview,
            (Page::Overview, Some(_)) if !compact => Page::Harness,
            (page, Some(_)) => page,
        };
        let body = match (page, kind) {
            (Page::Overview, _) | (_, None) => self.overview(window, cx),
            (Page::Harness, Some(kind)) => self.harness_page(kind, stacked, window, cx),
            (Page::Browse, Some(kind)) => self.browse_page(kind, stacked, window, cx),
        };
        v_flex()
            .w_full()
            .gap(if compact {
                cx.theme().tokens.spacing.lg
            } else {
                material::section_gap(cx)
            })
            .on_action(cx.listener(|this, action: &RunPluginAction, _, cx| {
                this.run(
                    &action.profile_id,
                    &action.entry_id,
                    action.action,
                    action.cwd.clone(),
                    cx,
                );
            }))
            .on_action(cx.listener(|this, action: &ChooseProject, _, cx| {
                this.project = Some(action.project_id.clone());
                this.list_missing(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, action: &RemoveMarketplace, _, cx| {
                let (profile_id, name) = (action.profile_id.clone(), action.name.clone());
                this.store.update(cx, |store, _| {
                    store.remove_provider_marketplace(profile_id, name, None)
                });
            }))
            .child(body)
    }
}

impl PluginsSettingsPanel {}

mod browse;
mod pages;
mod parts;
mod project;
mod rows;
use parts::*;

#[cfg(test)]
mod tests;
