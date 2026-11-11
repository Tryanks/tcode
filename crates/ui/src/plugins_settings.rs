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
    PluginCatalogState, PluginChallenge, PluginChallengeKind, ProviderPluginCatalog,
    RuntimeOperationId,
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
        let profiles = self.store.read(cx).enabled_profiles();
        ProviderKind::NATIVE
            .into_iter()
            .filter(|kind| kind.caps().plugin_management.manages())
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
            let management = profile.kind.caps().plugin_management;
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

impl PluginsSettingsPanel {
    /// The list of Harnesses: the first page on a compact window, and what
    /// shows when none is managed.
    fn overview(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let harnesses = self.harnesses(cx);
        let rows: Vec<AnyElement> = harnesses
            .iter()
            .map(|kind| {
                let kind = *kind;
                let accounts = self.accounts(kind, cx);
                let installed = rows(&accounts, false, PluginScope::User).len();
                let id = format!(
                    "plugins-harness-{}",
                    tcode_core::settings::provider_key(kind)
                );
                let subtitle = crate::tr!(
                    "providers.plugins.harness_summary",
                    accounts = accounts.len(),
                    plugins = installed
                );
                material::list_row(
                    SharedString::from(id.clone()),
                    kind.display_name().into(),
                    cx,
                )
                .debug_selector(move || id.clone())
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_body(self.touch, cx).child(kind.display_name()))
                        .child(
                            div()
                                .text_secondary(self.touch, cx)
                                .child(subtitle.into_owned()),
                        ),
                )
                .child(Icon::new(IconName::ChevronRight).text_color(cx.theme().muted_foreground))
                .on_click(cx.listener(move |this, _, _, cx| this.open(Page::Harness, kind, cx)))
                .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        v_flex()
            .w_full()
            .gap(cx.theme().tokens.spacing.lg)
            .when(!self.touch, |page| {
                page.child(material::group_caption(
                    crate::tr!("settings.plugins_section"),
                    cx,
                ))
            })
            .child(if empty {
                material::grouped(
                    vec![note_row(
                        crate::tr!("providers.plugins.no_providers").into_owned(),
                        cx,
                    )],
                    cx,
                )
                .into_any_element()
            } else {
                material::grouped(rows, cx).into_any_element()
            })
            .children(self.other_harnesses(cx))
            .into_any_element()
    }

    /// One honest line for each Harness with an account whose plugins Tcode
    /// does not manage.
    fn other_harnesses(&self, cx: &App) -> Option<AnyElement> {
        let profiles = self.store.read(cx).enabled_profiles();
        let rows: Vec<AnyElement> = ProviderKind::NATIVE
            .into_iter()
            .filter(|kind| profiles.iter().any(|profile| profile.kind == *kind))
            .filter_map(|kind| {
                let unmanaged = kind.caps().plugin_management.unmanaged?;
                let name = kind.display_name();
                let line = match unmanaged.kind {
                    UnmanagedPluginsKind::EditorOnly => {
                        crate::tr!("providers.plugins.unmanaged.editor_only", name = name)
                    }
                };
                let id = format!("plugins-other-{}", tcode_core::settings::provider_key(kind));
                Some(
                    material::group_row_stacked(cx)
                        .debug_selector(move || id.clone())
                        .gap_0p5()
                        .child(div().text_body(self.touch, cx).child(name))
                        .child(
                            div()
                                .text_secondary(self.touch, cx)
                                .child(line.into_owned()),
                        )
                        .into_any_element(),
                )
            })
            .collect();
        (!rows.is_empty()).then(|| {
            v_flex()
                .w_full()
                .child(material::group_caption(
                    crate::tr!("providers.plugins.other_harnesses"),
                    cx,
                ))
                .child(material::grouped(rows, cx))
                .into_any_element()
        })
    }

    fn back_bar(&self, label: String, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .child(
                Button::new("plugins-back")
                    .debug_selector(|| "plugins-back".into())
                    .ghost()
                    .when(!compact, |button| button.small())
                    .icon(IconName::ChevronLeft)
                    .label(label)
                    .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
            )
            .into_any_element()
    }

    fn harness_tabs(&self, kind: ProviderKind, cx: &mut Context<Self>) -> AnyElement {
        let tabs = self
            .harnesses(cx)
            .into_iter()
            .map(|harness| {
                let installed = rows(&self.accounts(harness, cx), false, PluginScope::User).len();
                let key = tcode_core::settings::provider_key(harness);
                TabItem::new(
                    SharedString::from(format!("plugins-tab-{key}")),
                    harness.display_name(),
                )
                .count(installed)
                .selected(harness == kind)
                .on_click(cx.listener(move |this, _, _, cx| this.open(Page::Harness, harness, cx)))
            })
            .collect::<Vec<_>>();
        TabBar::new("plugins-harness-tabs")
            .underline()
            .w_full()
            .aria_label(crate::tr!("settings.plugins"))
            .items(tabs)
            .into_any_element()
    }

    fn harness_page(
        &mut self,
        kind: ProviderKind,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let management = kind.caps().plugin_management;
        let accounts = self.accounts(kind, cx);
        let columns: Vec<Account> = if accounts.len() > MAX_COLUMNS && !compact {
            let shown = self
                .column
                .as_ref()
                .and_then(|id| accounts.iter().find(|account| account.id == *id))
                .or(accounts.first())
                .cloned();
            shown.into_iter().collect()
        } else {
            accounts.clone()
        };
        let user_rows = rows(&columns, false, PluginScope::User);
        let managed_rows = rows(&columns, false, PluginScope::Managed);
        let has_project = management.scopes.contains(&PluginScope::Project);
        let project = has_project.then(|| self.project(cx)).flatten();

        let mut page = v_flex().w_full().gap(if compact {
            cx.theme().tokens.spacing.lg
        } else {
            material::section_gap(cx)
        });
        if self.touch {
            page = page.child(self.back_bar(
                crate::tr!("settings.plugins").into_owned(),
                self.touch,
                cx,
            ));
        } else {
            page = page.child(self.harness_tabs(kind, cx));
        }
        page = page.child(self.harness_header(kind, &accounts, compact, window, cx));
        if accounts.len() > MAX_COLUMNS && !compact {
            page = page.child(self.column_switcher(&accounts, &columns, cx));
        }
        page = page.children(self.failures(&accounts, kind, cx));

        // User scope: one switch per account.
        let caption = crate::tr!("providers.plugins.user_scope").into_owned();
        let note = (!management.supports(PluginActionKind::Enable)).then(|| {
            crate::tr!(
                "providers.plugins.no_enablement",
                name = kind.display_name()
            )
            .into_owned()
        });
        let listing = columns.iter().all(|account| {
            account.user.is_none()
                || (account.loading()
                    && account
                        .user
                        .as_ref()
                        .is_some_and(|catalog| catalog.entries.is_empty()))
        });
        let body = if user_rows.is_empty() {
            if listing {
                note_card(crate::tr!("providers.plugins.listing").into_owned(), cx)
            } else {
                self.empty_user(kind, &accounts, cx)
            }
        } else {
            self.plugin_rows(
                &columns,
                user_rows,
                false,
                PluginScope::User,
                kind,
                compact,
                window,
                cx,
            )
        };
        page = page.child(self.section(caption, None, &columns, compact, body, note, cx));

        if has_project {
            page = page.child(self.project_section(kind, &columns, project, compact, window, cx));
        }
        if !managed_rows.is_empty() {
            let body = self.plugin_rows(
                &columns,
                managed_rows,
                false,
                PluginScope::Managed,
                kind,
                compact,
                window,
                cx,
            );
            page = page.child(self.section(
                crate::tr!("providers.plugins.managed_scope").into_owned(),
                None,
                &columns,
                compact,
                body,
                None,
                cx,
            ));
        }
        if management.marketplaces {
            page = page.child(self.marketplaces(&columns, &accounts, compact, cx));
        }
        if !self.touch {
            page = page.children(self.other_harnesses(cx));
        }
        page.into_any_element()
    }

    fn harness_header(
        &self,
        kind: ProviderKind,
        accounts: &[Account],
        compact: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let chips: Vec<_> = accounts
            .iter()
            .map(|account| {
                let state: AnyElement = if account.loading() {
                    Spinner::new().xsmall().color(muted).into_any_element()
                } else if account.failure().is_some() {
                    Icon::new(IconName::TriangleAlert)
                        .xsmall()
                        .text_color(cx.theme().danger)
                        .into_any_element()
                } else {
                    div().into_any_element()
                };
                let id = format!("plugins-account-{}", account.id);
                material::chip_surface(cx.theme().secondary, cx.theme().foreground, cx)
                    .debug_selector(move || id.clone())
                    .child(material::status_dot(account.color))
                    .child(account.name.clone())
                    .child(state)
            })
            .collect();
        let refresh = Button::new("plugins-refresh")
            .debug_selector(|| "plugins-refresh".into())
            .ghost()
            .small()
            .icon(IconName::RotateCcw)
            .tooltip(crate::tr!(
                "providers.plugins.refresh",
                name = kind.display_name()
            ))
            .on_click(cx.listener(move |this, _, _, cx| this.relist(kind, cx)));
        let installs = kind
            .caps()
            .plugin_management
            .supports(PluginActionKind::Install);
        let browse = installs.then(|| {
            self.install_button("plugins-browse", kind, accounts, compact, cx)
                .debug_selector(|| "plugins-browse".into())
        });
        let description = div()
            .text_secondary(self.touch, cx)
            .child(if kind.caps().home_path {
                crate::tr!("providers.plugins.where_kept").into_owned()
            } else {
                crate::tr!(
                    "providers.plugins.where_kept_shared",
                    name = kind.display_name()
                )
                .into_owned()
            });
        if compact {
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .children(browse.map(|b| div().flex_1().child(b)))
                        .child(refresh),
                )
                .child(description)
                .child(h_flex().w_full().flex_wrap().gap_1p5().children(chips))
                .into_any_element()
        } else {
            h_flex()
                .w_full()
                .items_start()
                .gap_3()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .child(description)
                        .child(h_flex().w_full().flex_wrap().gap_1p5().children(chips)),
                )
                .child(refresh)
                .children(browse)
                .into_any_element()
        }
    }

    fn column_switcher(
        &self,
        accounts: &[Account],
        columns: &[Account],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = columns.first().map(|account| account.id.clone());
        let items = accounts.iter().map(|account| {
            let id = account.id.clone();
            TabItem::new(
                SharedString::from(format!("plugins-column-{id}")),
                account.name.clone(),
            )
            .selected(shown.as_ref() == Some(&account.id))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.column = Some(id.clone());
                cx.notify();
            }))
        });
        TabBar::new("plugins-columns")
            .segmented()
            .items(items.collect::<Vec<_>>())
            .into_any_element()
    }

    /// A notice for each account whose listing failed, with its native text.
    fn failures(
        &self,
        accounts: &[Account],
        kind: ProviderKind,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        for account in accounts {
            let errors: Vec<String> = account
                .failure()
                .into_iter()
                .chain(
                    [&account.user, &account.project]
                        .into_iter()
                        .flatten()
                        .flat_map(|catalog| catalog.errors.iter().cloned()),
                )
                .collect();
            if errors.is_empty() {
                continue;
            }
            let failed = account.failure().is_some();
            let tone = if failed {
                cx.theme().danger
            } else {
                cx.theme().warning
            };
            let id = format!("plugins-failure-{}", account.id);
            notices.push(
                material::notice(tone, cx)
                    .debug_selector(move || id.clone())
                    .child(
                        h_flex()
                            .w_full()
                            .items_start()
                            .gap_3()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(div().text_body(false, cx).child(if failed {
                                        crate::tr!(
                                            "providers.plugins.list_failed",
                                            name = account.name
                                        )
                                        .into_owned()
                                    } else {
                                        crate::tr!(
                                            "providers.plugins.catalog_errors",
                                            name = account.name
                                        )
                                        .into_owned()
                                    }))
                                    .children(errors.into_iter().map(|error| {
                                        div().text_meta_mono(false, cx).child(verbatim(error, cx))
                                    })),
                            )
                            .when(failed, |row| {
                                row.child(
                                    Button::new(SharedString::from(format!(
                                        "plugins-retry-{}",
                                        account.id
                                    )))
                                    .outline()
                                    .small()
                                    .label(crate::tr!("providers.plugins.retry").into_owned())
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.relist(kind, cx)),
                                    ),
                                )
                            }),
                    )
                    .into_any_element(),
            );
        }
        notices
    }

    /// The way into installing: a harness without a catalog takes a package
    /// source, any other browses its catalog.
    fn install_button(
        &self,
        id: &'static str,
        kind: ProviderKind,
        accounts: &[Account],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Button {
        let management = kind.caps().plugin_management;
        let by_source = management.source_install;
        let accounts = accounts.to_vec();
        let project = management
            .scopes
            .contains(&PluginScope::Project)
            .then(|| self.project_root(cx))
            .flatten();
        Button::new(id)
            .primary()
            .when(!compact, |button| button.small())
            .when(compact, |button| button.w_full())
            .icon(if by_source {
                IconName::Plus
            } else {
                IconName::Search
            })
            .label(if by_source {
                crate::tr!("providers.plugins.add_package").into_owned()
            } else {
                crate::tr!("providers.plugins.browse").into_owned()
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if by_source {
                    this.open_add_source(
                        AddedSource::Package,
                        accounts.clone(),
                        project.clone(),
                        window,
                        cx,
                    );
                } else {
                    this.open(Page::Browse, kind, cx);
                }
            }))
    }

    fn empty_user(
        &self,
        kind: ProviderKind,
        accounts: &[Account],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let installs = kind
            .caps()
            .plugin_management
            .supports(PluginActionKind::Install);
        material::grouped(
            vec![
                material::group_row_stacked(cx)
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_secondary(self.touch, cx)
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                crate::tr!(
                                    "providers.plugins.no_installed",
                                    name = kind.display_name()
                                )
                                .into_owned(),
                            ),
                    )
                    .when(installs, |col| {
                        col.child(self.install_button(
                            "plugins-empty-browse",
                            kind,
                            accounts,
                            false,
                            cx,
                        ))
                    })
                    .into_any_element(),
            ],
            cx,
        )
        .into_any_element()
    }

    /// A captioned group; wide, the caption carries a heading per column.
    #[allow(clippy::too_many_arguments)]
    fn section(
        &self,
        caption: String,
        lead: Option<AnyElement>,
        columns: &[Account],
        compact: bool,
        body: AnyElement,
        note: Option<String>,
        cx: &App,
    ) -> AnyElement {
        let headings = (!compact).then(|| {
            h_flex()
                .flex_none()
                .children(columns.iter().map(|account| {
                    h_flex()
                        .w(px(CELL_WIDTH))
                        .justify_center()
                        .gap_1()
                        .overflow_hidden()
                        .child(material::status_dot(account.color))
                        .child(
                            div()
                                .truncate()
                                .text_caption(cx)
                                .child(account.name.clone()),
                        )
                }))
                .child(div().w(px(MENU_WIDTH)))
        });
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .pl(px(material::CARD_INSET))
                    .pr(px(material::CARD_INSET))
                    .pb(cx.theme().tokens.spacing.sm)
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .text_caption(cx)
                            .child(caption)
                            .children(lead),
                    )
                    .children(headings),
            )
            .child(body)
            .children(note.map(|note| {
                div()
                    .pt_2()
                    .pl(px(material::CARD_INSET))
                    .text_secondary(self.touch, cx)
                    .text_color(cx.theme().muted_foreground)
                    .child(note)
            }))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn plugin_rows(
        &mut self,
        columns: &[Account],
        rows: Vec<Row>,
        project: bool,
        scope: PluginScope,
        kind: ProviderKind,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Rows are keyed by plugin, so one installed or removed folds in or
        // out where it is and the rest glide to their places.
        let id = format!("plugin-rows-{}", scope_key(scope));
        let rows = rows
            .into_iter()
            .map(|row| (SharedString::from(row.entry.id.clone()), Rc::new(row)));
        if compact {
            let cards = crate::motion::keyed(
                SharedString::from(id),
                rows,
                ROWS,
                window,
                cx,
                |_, row, window, cx| {
                    div()
                        .w_full()
                        .pb_3()
                        .child(self.plugin_card(columns, row, project, scope, kind, window, cx))
                        .into_any_element()
                },
            );
            return cards.render(v_flex().w_full()).into_any_element();
        }
        let rows = crate::settings_rows::grouped_keyed(
            SharedString::from(id),
            rows,
            window,
            cx,
            |_, row, window, cx| self.plugin_row(columns, row, project, scope, kind, window, cx),
        );
        material::group(cx).child(rows).into_any_element()
    }

    /// What describes a plugin, the same in every account.
    fn plugin_summary(&self, row: &Row, scope: PluginScope, cx: &mut Context<Self>) -> AnyElement {
        let entry = &row.entry;
        let expanded = self.expanded.contains(&entry.id);
        let id = entry.id.clone();
        let toggle_id = format!("plugin-details-{}-{}", scope_key(scope), entry.id);
        let muted = cx.theme().muted_foreground;
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .child(
                h_flex()
                    .id(SharedString::from(toggle_id.clone()))
                    .debug_selector(move || toggle_id.clone())
                    .w_full()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded.remove(&id) {
                            this.expanded.insert(id.clone());
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_body(self.touch, cx)
                            .font_medium()
                            .child(entry.name.clone()),
                    )
                    .child(source_chip(entry, cx))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .text_color(muted),
                    ),
            )
            .when_some(entry.description.clone(), |col, description| {
                col.child(div().text_secondary(self.touch, cx).child(description))
            })
            .children(components_line(entry, self.touch, cx))
            .children(entry.errors.iter().map(|error| {
                div()
                    .text_secondary(self.touch, cx)
                    .text_color(cx.theme().danger)
                    .child(verbatim(error.clone(), cx))
            }))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn plugin_row(
        &self,
        columns: &[Account],
        row: &Row,
        project: bool,
        scope: PluginScope,
        kind: ProviderKind,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row_id = format!("plugin-row-{}-{}", scope_key(scope), row.entry.id);
        let differ = row.versions_differ(scope);
        let right: AnyElement = if project {
            self.project_switches(columns, row, cx)
        } else {
            h_flex()
                .flex_none()
                .items_start()
                .children(columns.iter().zip(&row.cells).map(|(account, cell)| {
                    div()
                        .w(px(CELL_WIDTH))
                        .flex()
                        .justify_center()
                        .child(self.cell(
                            account,
                            cell.as_ref(),
                            &row.entry,
                            scope,
                            differ,
                            false,
                            cx,
                        ))
                }))
                .into_any_element()
        };
        let expanded = self.expanded.contains(&row.entry.id);
        let summary = self.plugin_summary(row, scope, cx);
        let readiness = project.then(|| self.readiness(columns, row, kind, cx));
        material::group_row_stacked(cx)
            .id(SharedString::from(row_id.clone()))
            .debug_selector(move || row_id.clone())
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_3()
                    .child(
                        Icon::new(IconName::Puzzle)
                            .text_color(cx.theme().muted_foreground)
                            .mt_0p5(),
                    )
                    .child(summary)
                    .child(right)
                    .child(self.row_menu(columns, row, project, scope, cx)),
            )
            .children(readiness)
            .child(
                Collapsible::new(
                    SharedString::from(format!("plugin-more-{}", row.entry.id)),
                    expanded,
                )
                .motion(&self.motion)
                .w_full()
                .content(details(columns, row, project, scope, cx)),
            )
            .into_any_element()
    }

    /// A phone's card for one plugin: what describes it, then a row per
    /// account, or one row for the project every account shares.
    #[allow(clippy::too_many_arguments)]
    fn plugin_card(
        &self,
        columns: &[Account],
        row: &Row,
        project: bool,
        scope: PluginScope,
        kind: ProviderKind,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row_id = format!("plugin-row-{}-{}", scope_key(scope), row.entry.id);
        let differ = row.versions_differ(scope);
        let expanded = self.expanded.contains(&row.entry.id);
        let mut lines = vec![
            material::group_row_stacked(cx)
                .gap_1()
                .child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap_2()
                        .child(self.plugin_summary(row, scope, cx))
                        .child(self.row_menu(columns, row, project, scope, cx)),
                )
                .child(
                    Collapsible::new(
                        SharedString::from(format!("plugin-more-{}", row.entry.id)),
                        expanded,
                    )
                    .motion(&self.motion)
                    .w_full()
                    .content(details(columns, row, project, scope, cx)),
                )
                .into_any_element(),
        ];
        if project {
            lines.push(
                material::group_row(cx)
                    .child(
                        div().flex_1().min_w_0().text_body(self.touch, cx).child(
                            crate::tr!("providers.plugins.this_project_shared").into_owned(),
                        ),
                    )
                    .child(self.project_switches(columns, row, cx))
                    .into_any_element(),
            );
            lines.push(
                material::group_row_stacked(cx)
                    .child(self.readiness(columns, row, kind, cx))
                    .into_any_element(),
            );
        } else {
            for (account, cell) in columns.iter().zip(&row.cells) {
                lines.push(
                    material::group_row(cx)
                        .when(self.touch, |row| row.min_h(px(material::TOUCH_TARGET)))
                        .gap_2()
                        .child(material::status_dot(account.color))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_body(self.touch, cx)
                                .child(account.name.clone()),
                        )
                        .child(self.cell(
                            account,
                            cell.as_ref(),
                            &row.entry,
                            scope,
                            differ,
                            true,
                            cx,
                        ))
                        .into_any_element(),
                );
            }
        }
        div()
            .id(SharedString::from(row_id.clone()))
            .debug_selector(move || row_id.clone())
            .w_full()
            .child(material::grouped(lines, cx))
            .into_any_element()
    }

    /// One account's state for a plugin in its own scope.
    #[allow(clippy::too_many_arguments)]
    fn cell(
        &self,
        account: &Account,
        entry: Option<&ProviderPluginEntry>,
        described: &ProviderPluginEntry,
        scope: PluginScope,
        differ: bool,
        inline: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let cell_id = format!("plugin-cell-{}-{}", account.id, described.id);
        let busy = account.busy();
        let lines = if inline {
            h_flex().items_center().gap_2()
        } else {
            v_flex().items_center().gap_0p5()
        };
        let lines = lines.debug_selector({
            let cell_id = cell_id.clone();
            move || cell_id.clone()
        });
        let Some(catalog) = account.user.as_ref() else {
            return lines
                .child(Spinner::new().small().color(muted))
                .into_any_element();
        };
        let Some(entry) = entry else {
            if catalog.loading && catalog.entries.is_empty() {
                return lines
                    .child(Spinner::new().small().color(muted))
                    .into_any_element();
            }
            let reason = match described.source.marketplace.as_deref() {
                Some(marketplace)
                    if !catalog
                        .marketplaces
                        .iter()
                        .any(|known| known.name == marketplace) =>
                {
                    crate::tr!(
                        "providers.plugins.no_marketplace",
                        name = account.name,
                        marketplace = marketplace
                    )
                }
                _ => crate::tr!("providers.plugins.not_offered", name = account.name),
            };
            return lines
                .child(
                    div()
                        .id(SharedString::from(format!("{cell_id}-none")))
                        .text_color(muted)
                        .child("—")
                        .with_tooltip({
                            let reason = reason.into_owned();
                            move |_, _| Tooltip::new(reason.clone())
                        }),
                )
                .into_any_element();
        };
        let installed = installation(entry, scope);
        let pending = account.pending(&entry.id);
        let enable = PluginAction::Enable { scope };
        let disable = PluginAction::Disable { scope };
        let toggles = offers(entry, enable) || offers(entry, disable);
        let control: AnyElement = if pending {
            Spinner::new().small().color(muted).into_any_element()
        } else if installed.is_some() && toggles {
            let on = scope_enabled(entry, scope);
            let (account_id, entry_id) = (account.id.clone(), entry.id.clone());
            let action = if on { disable } else { enable };
            let switch_id = format!("plugin-switch-{}-{}", account.id, entry.id);
            div()
                .debug_selector(move || switch_id.clone())
                .child(
                    Switch::new(SharedString::from(format!(
                        "plugin-switch-{}-{}",
                        account.id, entry.id
                    )))
                    .checked(on)
                    .disabled(busy || !offers(entry, action))
                    .tooltip(account.name.clone())
                    .on_click(cx.listener(
                        move |this, checked: &bool, _, cx| {
                            let action = if *checked { enable } else { disable };
                            this.run(&account_id, &entry_id, action, None, cx);
                        },
                    )),
                )
                .into_any_element()
        } else if installed.is_some() {
            material::semantic_chip(
                crate::tr!("providers.plugins.installed_state").into_owned(),
                cx.theme().muted,
                muted,
                cx,
            )
            .into_any_element()
        } else if offers(entry, PluginAction::Install { scope }) {
            let (account_id, entry_id) = (account.id.clone(), entry.id.clone());
            let id = format!("plugin-install-{}-{}", account.id, entry.id);
            Button::new(SharedString::from(id.clone()))
                .debug_selector(move || id.clone())
                .outline()
                .small()
                .disabled(busy)
                .label(crate::tr!("providers.plugins.action.install").into_owned())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run(
                        &account_id,
                        &entry_id,
                        PluginAction::Install { scope },
                        None,
                        cx,
                    )
                }))
                .into_any_element()
        } else {
            div().text_color(muted).child("—").into_any_element()
        };
        let version = installed.and_then(|installation| installation.version.clone());
        let fact: Option<AnyElement> = match (&entry.latest_version, installed) {
            (Some(latest), Some(_)) => Some(
                div()
                    .text_token(&cx.theme().tokens.typography.xs)
                    .text_color(cx.theme().primary)
                    .child(crate::tr!("providers.plugins.latest", version = latest).into_owned())
                    .into_any_element(),
            ),
            (None, Some(_)) if differ => Some(
                div()
                    .text_token(&cx.theme().tokens.typography.xs)
                    .text_color(cx.theme().warning)
                    .child(crate::tr!("providers.plugins.versions_differ").into_owned())
                    .into_any_element(),
            ),
            (_, None) if !inline => Some(
                div()
                    .text_token(&cx.theme().tokens.typography.xs)
                    .text_color(muted)
                    .child(crate::tr!("providers.plugins.not_installed").into_owned())
                    .into_any_element(),
            ),
            _ => None,
        };
        let update = PluginAction::Update { scope };
        let update_button = (entry.latest_version.is_some() && offers(entry, update) && !pending)
            .then(|| {
                let (account_id, entry_id) = (account.id.clone(), entry.id.clone());
                Button::new(SharedString::from(format!(
                    "plugin-update-{}-{}",
                    account.id, entry.id
                )))
                .outline()
                .xsmall()
                .disabled(busy)
                .label(crate::tr!("providers.plugins.action.update").into_owned())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run(&account_id, &entry_id, update, None, cx)
                }))
            });
        let version = version.map(|version| div().text_meta_mono(inline, cx).child(version));
        if inline {
            lines
                .children(fact)
                .children(version)
                .children(update_button)
                .child(control)
                .into_any_element()
        } else {
            lines
                .child(control)
                .children(version)
                .children(fact)
                .children(update_button)
                .into_any_element()
        }
    }

    /// The project's own switch for each project scope the plugin is in,
    /// changed through any account that offers it: the file is shared.
    fn project_switches(
        &self,
        columns: &[Account],
        row: &Row,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = self.project_root(cx);
        let muted = cx.theme().muted_foreground;
        let scopes = [PluginScope::Project, PluginScope::Local];
        let pending = columns.iter().any(|account| account.pending(&row.entry.id));
        let busy = columns.iter().any(Account::busy);
        let switches = scopes.into_iter().filter_map(|scope| {
            let holder = row
                .cells
                .iter()
                .flatten()
                .find(|entry| installation(entry, scope).is_some())?;
            let on = scope_enabled(holder, scope);
            let wanted = if on {
                PluginAction::Disable { scope }
            } else {
                PluginAction::Enable { scope }
            };
            let via = columns
                .iter()
                .zip(&row.cells)
                .find(|(_, cell)| cell.as_ref().is_some_and(|entry| offers(entry, wanted)))
                .map(|(account, _)| account.id.clone());
            let label = match scope {
                PluginScope::Local => crate::tr!("providers.plugins.local_only"),
                _ => crate::tr!("providers.plugins.this_project"),
            };
            let id = format!(
                "plugin-project-switch-{}-{}",
                scope_key(scope),
                row.entry.id
            );
            let entry_id = row.entry.id.clone();
            let root = root.clone();
            Some(
                h_flex()
                    .gap_2()
                    .items_center()
                    .justify_end()
                    .child(
                        div()
                            .text_secondary(false, cx)
                            .text_color(muted)
                            .child(label.into_owned()),
                    )
                    .child(
                        div().debug_selector(move || id.clone()).child(
                            Switch::new(SharedString::from(format!(
                                "plugin-project-switch-{}-{}",
                                scope_key(scope),
                                row.entry.id
                            )))
                            .checked(on)
                            .disabled(busy || via.is_none())
                            .on_click(cx.listener(
                                move |this, checked: &bool, _, cx| {
                                    let Some(via) = via.clone() else {
                                        return;
                                    };
                                    let action = if *checked {
                                        PluginAction::Enable { scope }
                                    } else {
                                        PluginAction::Disable { scope }
                                    };
                                    this.run(&via, &entry_id, action, root.clone(), cx);
                                },
                            )),
                        ),
                    ),
            )
        });
        v_flex()
            .flex_none()
            .items_end()
            .gap_1()
            .when(pending, |col| {
                col.child(Spinner::new().small().color(muted))
            })
            .children(switches)
            .into_any_element()
    }

    /// Whether each account's sessions in the project get the plugin: the
    /// enablement is shared, the files are each account's own.
    fn readiness(
        &self,
        columns: &[Account],
        row: &Row,
        kind: ProviderKind,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = self.project_root(cx);
        let muted = cx.theme().muted_foreground;
        let items = columns.iter().zip(&row.cells).map(|(account, cell)| {
            let ready = cell.as_ref().and_then(|entry| {
                installation(entry, PluginScope::Project)
                    .or(installation(entry, PluginScope::Local))
            });
            let install = cell.as_ref().is_some_and(|entry| {
                offers(
                    entry,
                    PluginAction::Install {
                        scope: PluginScope::Project,
                    },
                )
            });
            let id = format!("plugin-ready-{}-{}", account.id, row.entry.id);
            // A short state beside the account, and for an account the
            // project's plugin does not reach, what the Harness does then.
            let (state, color, why) = match ready {
                Some(installation) => (
                    match &installation.version {
                        Some(version) => {
                            crate::tr!("providers.plugins.ready_version", version = version)
                        }
                        None => crate::tr!("providers.plugins.ready"),
                    },
                    cx.theme().success,
                    None,
                ),
                None if install => (
                    crate::tr!("providers.plugins.not_installed"),
                    cx.theme().warning,
                    Some(crate::tr!(
                        "providers.plugins.not_installed_here",
                        harness = kind.display_name()
                    )),
                ),
                None => (
                    crate::tr!("providers.plugins.cannot_install"),
                    cx.theme().danger,
                    Some(match row.entry.source.marketplace.as_deref() {
                        Some(marketplace) => crate::tr!(
                            "providers.plugins.cannot_install_here",
                            marketplace = marketplace
                        ),
                        None => crate::tr!("providers.plugins.not_offered", name = account.name),
                    }),
                ),
            };
            let (account_id, entry_id, root) =
                (account.id.clone(), row.entry.id.clone(), root.clone());
            let install_button = (ready.is_none() && install).then(|| {
                let button_id = format!("plugin-ready-install-{}-{}", account.id, row.entry.id);
                Button::new(SharedString::from(button_id.clone()))
                    .debug_selector(move || button_id.clone())
                    .ghost()
                    .xsmall()
                    .disabled(account.busy())
                    .label(
                        crate::tr!("providers.plugins.install_in", name = account.name)
                            .into_owned(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.run(
                            &account_id,
                            &entry_id,
                            PluginAction::Install {
                                scope: PluginScope::Project,
                            },
                            root.clone(),
                            cx,
                        )
                    }))
            });
            v_flex()
                .debug_selector(move || id.clone())
                .w_full()
                .gap_0p5()
                .child(
                    h_flex()
                        .w_full()
                        .flex_wrap()
                        .items_center()
                        .gap_1p5()
                        .child(material::status_dot(account.color))
                        .child(div().text_color(muted).child(account.name.clone()))
                        .child(div().text_color(color).child(state.into_owned()))
                        .children(install_button),
                )
                .children(why.map(|why| {
                    div()
                        .w_full()
                        .pl_3p5()
                        .text_color(muted)
                        .child(why.into_owned())
                }))
        });
        v_flex()
            .w_full()
            .gap_1()
            .text_secondary(self.touch, cx)
            .children(items)
            .into_any_element()
    }

    /// The row's menu: each account's update and uninstall the host offered.
    fn row_menu(
        &self,
        columns: &[Account],
        row: &Row,
        project: bool,
        scope: PluginScope,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cwd = if project { self.project_root(cx) } else { None };
        let scopes: Vec<PluginScope> = if project {
            vec![PluginScope::Project, PluginScope::Local]
        } else {
            vec![scope]
        };
        let mut items: Vec<(String, RunPluginAction)> = Vec::new();
        for (account, cell) in columns.iter().zip(&row.cells) {
            let Some(entry) = cell else {
                continue;
            };
            for &scope in &scopes {
                for (action, key) in [
                    (
                        PluginAction::Update { scope },
                        if entry.latest_version.is_some() {
                            "providers.plugins.menu.update"
                        } else {
                            "providers.plugins.menu.check_update"
                        },
                    ),
                    (
                        PluginAction::Uninstall { scope },
                        "providers.plugins.menu.uninstall",
                    ),
                ] {
                    if offers(entry, action) {
                        items.push((
                            crate::tr!(key, name = account.name).into_owned(),
                            RunPluginAction {
                                profile_id: account.id.clone(),
                                entry_id: entry.id.clone(),
                                action,
                                cwd: cwd.clone(),
                            },
                        ));
                    }
                }
            }
        }
        let id = format!("plugin-menu-{}-{}", scope_key(scope), row.entry.id);
        if items.is_empty() {
            return div().w(px(MENU_WIDTH)).flex_none().into_any_element();
        }
        let busy = columns.iter().any(Account::busy);
        div()
            .w(px(MENU_WIDTH))
            .flex_none()
            .debug_selector({
                let id = id.clone();
                move || id.clone()
            })
            .child(
                Button::new(SharedString::from(id))
                    .ghost()
                    .xsmall()
                    .disabled(busy)
                    .icon(IconName::Ellipsis)
                    .tooltip(
                        crate::tr!("providers.plugins.more", name = row.entry.name).into_owned(),
                    )
                    .dropdown_menu(move |mut menu, _, _| {
                        for (label, action) in &items {
                            menu = menu.menu(label.clone(), Box::new(action.clone()));
                        }
                        menu
                    }),
            )
            .into_any_element()
    }

    fn project_section(
        &mut self,
        kind: ProviderKind,
        columns: &[Account],
        project: Option<tcode_core::project::Project>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let projects = self.store.read(cx).projects();
        let picked = project.as_ref().map(|project| project.id.clone());
        let label = project
            .as_ref()
            .map(|project| project.name.clone())
            .unwrap_or_else(|| crate::tr!("providers.plugins.pick_project").into_owned());
        let picker = Button::new("plugins-project-picker")
            .debug_selector(|| "plugins-project-picker".into())
            .outline()
            .xsmall()
            .icon(IconName::Folder)
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                for project in &projects {
                    menu = menu.menu_with_check(
                        project.name.clone(),
                        picked.as_ref() == Some(&project.id),
                        Box::new(ChooseProject {
                            project_id: project.id.clone(),
                        }),
                    );
                }
                menu
            });
        let lead = h_flex()
            .items_center()
            .gap_2()
            .child(picker)
            .child(crate::tr!("providers.plugins.project_shared").into_owned())
            .into_any_element();
        let body = match &project {
            None => note_card(
                crate::tr!("providers.plugins.project_needed").into_owned(),
                cx,
            ),
            Some(_) => {
                let project_rows = {
                    let mut rows_found = rows(columns, true, PluginScope::Project);
                    for row in rows(columns, true, PluginScope::Local) {
                        if !rows_found
                            .iter()
                            .any(|found| found.entry.id == row.entry.id)
                        {
                            rows_found.push(row);
                        }
                    }
                    rows_found
                };
                let listing = columns.iter().all(|account| {
                    account
                        .project
                        .as_ref()
                        .is_none_or(|catalog| catalog.loading && catalog.entries.is_empty())
                });
                if !project_rows.is_empty() {
                    self.plugin_rows(
                        columns,
                        project_rows,
                        true,
                        PluginScope::Project,
                        kind,
                        compact,
                        window,
                        cx,
                    )
                } else if listing {
                    note_card(crate::tr!("providers.plugins.listing").into_owned(), cx)
                } else {
                    note_card(
                        crate::tr!("providers.plugins.no_project_plugins").into_owned(),
                        cx,
                    )
                }
            }
        };
        let note = (project.is_some()
            && kind
                .caps()
                .plugin_management
                .scopes
                .contains(&PluginScope::Local))
        .then(|| crate::tr!("providers.plugins.local_note").into_owned());
        let caption = crate::tr!("providers.plugins.project_scope").into_owned();
        // A project's plugins have one switch for every account, so the
        // caption carries no account headings.
        self.section(caption, Some(lead), &[], compact, body, note, cx)
    }

    fn marketplaces(
        &self,
        columns: &[Account],
        accounts: &[Account],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut names: Vec<ProviderPluginMarketplace> = Vec::new();
        for account in columns {
            for marketplace in account
                .user
                .iter()
                .flat_map(|catalog| &catalog.marketplaces)
            {
                if !names.iter().any(|known| known.name == marketplace.name) {
                    names.push(marketplace.clone());
                }
            }
        }
        let success = cx.theme().success;
        let muted = cx.theme().muted_foreground;
        let mut rows: Vec<AnyElement> = names
            .into_iter()
            .map(|marketplace| {
                let cells = columns.iter().map(|account| {
                    let catalog = account.user.as_ref();
                    let added = catalog.is_some_and(|catalog| {
                        catalog.marketplaces.iter().any(|known| known.name == marketplace.name)
                    });
                    let can_add = catalog.is_some_and(|catalog| {
                        catalog.marketplace_actions.contains(&MarketplaceAction::Add)
                    });
                    let pending = account.pending(&marketplace.name) || account.pending(&marketplace.source);
                    let id = format!("plugin-marketplace-{}-{}", account.id, marketplace.name);
                    let content: AnyElement = if pending {
                        Spinner::new().small().color(muted).into_any_element()
                    } else if added {
                        div()
                            .text_secondary(self.touch, cx)
                            .text_color(success)
                            .child(crate::tr!("providers.plugins.added").into_owned())
                            .into_any_element()
                    } else if can_add {
                        let (profile_id, source) = (account.id.clone(), marketplace.source.clone());
                        let store = self.store.clone();
                        Button::new(SharedString::from(format!("{id}-add")))
                            .outline()
                            .small()
                            .disabled(account.busy())
                            .label(crate::tr!("providers.plugins.add").into_owned())
                            .on_click(move |_, _, cx| {
                                let (profile_id, source) = (profile_id.clone(), source.clone());
                                store.update(cx, |store, _| {
                                    store.add_provider_marketplace(profile_id, source, None)
                                });
                            })
                            .into_any_element()
                    } else {
                        div().text_color(muted).child("—").into_any_element()
                    };
                    let cell = div().debug_selector(move || id.clone());
                    if compact {
                        cell.w_full()
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .items_center()
                                    .child(material::status_dot(account.color))
                                    .child(div().flex_1().min_w_0().truncate().child(account.name.clone()))
                                    .child(content),
                            )
                            .into_any_element()
                    } else {
                        cell.w(px(CELL_WIDTH)).flex().justify_center().child(content).into_any_element()
                    }
                });
                let removals: Vec<(String, RemoveMarketplace)> = columns
                    .iter()
                    .filter(|account| {
                        account.user.as_ref().is_some_and(|catalog| {
                            catalog.marketplace_actions.iter().any(|action| {
                                matches!(action, MarketplaceAction::Remove { marketplace: name, .. } if *name == marketplace.name)
                            })
                        })
                    })
                    .map(|account| {
                        (
                            crate::tr!("providers.plugins.menu.remove_marketplace", name = account.name).into_owned(),
                            RemoveMarketplace {
                                profile_id: account.id.clone(),
                                name: marketplace.name.clone(),
                            },
                        )
                    })
                    .collect();
                let menu: AnyElement = if removals.is_empty() {
                    div().w(px(MENU_WIDTH)).into_any_element()
                } else {
                    div()
                        .w(px(MENU_WIDTH))
                        .flex_none()
                        .child(
                            Button::new(SharedString::from(format!("plugin-marketplace-menu-{}", marketplace.name)))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Ellipsis)
                                .dropdown_menu(move |mut menu, _, _| {
                                    for (label, action) in &removals {
                                        menu = menu.menu(label.clone(), Box::new(action.clone()));
                                    }
                                    menu
                                }),
                        )
                        .into_any_element()
                };
                let describe = v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(div().text_body(self.touch, cx).child(marketplace.name.clone()))
                            .child(material::semantic_chip(
                                source_kind_label(marketplace.kind),
                                cx.theme().muted,
                                muted,
                                cx,
                            )),
                    )
                    .child(div().text_meta_mono(self.touch, cx).child(marketplace.source.clone()));
                if compact {
                    material::group_row_stacked(cx)
                        .gap_2()
                        .child(h_flex().w_full().items_start().gap_2().child(describe).child(menu))
                        .children(cells)
                        .into_any_element()
                } else {
                    material::group_row_stacked(cx)
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(Icon::new(IconName::Puzzle).text_color(muted))
                        .child(describe)
                        .child(h_flex().flex_none().children(cells))
                        .child(menu)
                        .into_any_element()
                }
            })
            .collect();
        let can_add = accounts.iter().any(|account| {
            account.user.as_ref().is_some_and(|catalog| {
                catalog
                    .marketplace_actions
                    .contains(&MarketplaceAction::Add)
            })
        });
        if can_add {
            let accounts = accounts.to_vec();
            rows.push(
                material::group_row(cx)
                    .child(
                        Button::new("plugin-marketplace-add")
                            .debug_selector(|| "plugin-marketplace-add".into())
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .label(crate::tr!("providers.plugins.add_marketplace").into_owned())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_add_source(
                                    AddedSource::Marketplace,
                                    accounts.clone(),
                                    None,
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .into_any_element(),
            );
        }
        let body = if rows.is_empty() {
            note_card(
                crate::tr!("providers.plugins.no_marketplaces").into_owned(),
                cx,
            )
        } else {
            material::grouped(rows, cx).into_any_element()
        };
        self.section(
            crate::tr!("providers.plugins.marketplaces").into_owned(),
            None,
            columns,
            compact,
            body,
            None,
            cx,
        )
    }

    fn browse_page(
        &mut self,
        kind: ProviderKind,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accounts = self.accounts(kind, cx);
        let query = self.search.read(cx).value().trim().to_lowercase();
        let mut entries: Vec<ProviderPluginEntry> = Vec::new();
        for account in &accounts {
            for entry in account.user.iter().flat_map(|catalog| &catalog.entries) {
                if !entries.iter().any(|known| known.id == entry.id) {
                    entries.push(entry.clone());
                }
            }
        }
        let mut marketplaces: Vec<(String, usize)> = Vec::new();
        for entry in &entries {
            if let Some(marketplace) = &entry.source.marketplace {
                match marketplaces
                    .iter_mut()
                    .find(|(name, _)| name == marketplace)
                {
                    Some((_, count)) => *count += 1,
                    None => marketplaces.push((marketplace.clone(), 1)),
                }
            }
        }
        let total = entries.len();
        let filter = self
            .marketplace_filter
            .clone()
            .filter(|name| marketplaces.iter().any(|(known, _)| known == name));
        let matches: Vec<ProviderPluginEntry> = entries
            .into_iter()
            .filter(|entry| filter.is_none() || entry.source.marketplace == filter)
            .filter(|entry| {
                query.is_empty()
                    || entry.name.to_lowercase().contains(&query)
                    || entry
                        .description
                        .as_deref()
                        .is_some_and(|text| text.to_lowercase().contains(&query))
            })
            .collect();
        let chips = std::iter::once((
            None,
            crate::tr!("providers.plugins.all").into_owned(),
            total,
        ))
        .chain(
            marketplaces
                .into_iter()
                .map(|(name, count)| (Some(name.clone()), name, count)),
        )
        .map(|(name, label, count)| {
            let id = format!("plugins-filter-{}", name.as_deref().unwrap_or("all"));
            let selected = filter == name;
            TabItem::new(SharedString::from(id), label)
                .count(count)
                .selected(selected)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.marketplace_filter = name.clone();
                    cx.notify();
                }))
        })
        .collect::<Vec<_>>();
        let listing = accounts.iter().any(|account| {
            account
                .user
                .as_ref()
                .is_none_or(|catalog| catalog.loading && catalog.entries.is_empty())
        });
        let body = if matches.is_empty() {
            note_card(
                if listing && total == 0 {
                    crate::tr!("providers.plugins.listing")
                } else if total == 0 {
                    crate::tr!("providers.plugins.no_available")
                } else {
                    crate::tr!("providers.plugins.no_matches")
                }
                .into_owned(),
                cx,
            )
        } else {
            let rows: Vec<AnyElement> = matches
                .iter()
                .map(|entry| self.browse_row(kind, &accounts, entry, compact, window, cx))
                .collect();
            material::grouped(rows, cx).into_any_element()
        };
        v_flex()
            .w_full()
            .gap_3()
            .child(self.back_bar(kind.display_name().to_string(), compact, cx))
            .child(
                div().text_title(cx).child(
                    crate::tr!("providers.plugins.browse_title", name = kind.display_name())
                        .into_owned(),
                ),
            )
            .child(Input::new(&self.search).placeholder(crate::tr!("providers.plugins.search")))
            .child(TabBar::new("plugins-filters").segmented().items(chips))
            .child(body)
            .into_any_element()
    }

    fn browse_row(
        &mut self,
        kind: ProviderKind,
        accounts: &[Account],
        entry: &ProviderPluginEntry,
        compact: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let installed_in: Vec<&Account> = accounts
            .iter()
            .filter(|account| {
                account
                    .entry(false, &entry.id)
                    .is_some_and(|entry| installation(entry, PluginScope::User).is_some())
            })
            .collect();
        let installable = accounts
            .iter()
            .any(|account| !install_scopes(account, &entry.id).is_empty());
        let pending = accounts.iter().any(|account| account.pending(&entry.id));
        let row_id = format!("plugin-available-{}", entry.id);
        let install: Option<AnyElement> = installable.then(|| {
            let open = self
                .install
                .as_ref()
                .is_some_and(|draft| draft.entry_id == entry.id);
            let trigger_id = format!("plugin-install-open-{}", entry.id);
            let entry_id = entry.id.clone();
            let accounts_for = accounts.to_vec();
            let trigger = Button::new(SharedString::from(trigger_id.clone()))
                .debug_selector(move || trigger_id.clone())
                .outline()
                .small()
                .label(crate::tr!("providers.plugins.install_more").into_owned())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.begin_install(entry_id.clone(), &accounts_for, cx)
                }));
            let panel = cx.entity();
            let accounts = accounts.to_vec();
            let popover = Popover::new(SharedString::from(format!("plugin-install-{}", entry.id)))
                .open(open)
                .anchor(gpui::Anchor::TopRight)
                .on_open_change({
                    let panel = panel.clone();
                    move |open, _, cx| {
                        if !*open {
                            panel.update(cx, |this, cx| {
                                this.install = None;
                                cx.notify();
                            });
                        }
                    }
                })
                .trigger(trigger);
            let popover = if self.touch {
                popover.bottom_sheet(
                    crate::tr!("providers.plugins.install_title", name = entry.name).into_owned(),
                )
            } else {
                popover
            };
            let (name, touch) = (entry.name.clone(), self.touch);
            let marketplace = entry.source.marketplace.clone();
            popover
                .content(move |_, _, cx| {
                    install_body(
                        &panel,
                        kind,
                        &accounts,
                        &name,
                        marketplace.as_deref(),
                        touch,
                        cx,
                    )
                })
                .into_any_element()
        });
        let badges = installed_in.iter().map(|account| {
            material::semantic_chip(
                crate::tr!("providers.plugins.installed_in", name = account.name).into_owned(),
                cx.theme().success.opacity(0.12),
                cx.theme().success,
                cx,
            )
        });
        material::group_row_stacked(cx)
            .id(SharedString::from(row_id.clone()))
            .debug_selector(move || row_id.clone())
            .flex_row()
            .items_start()
            .gap_3()
            .when(!compact, |row| {
                row.child(Icon::new(IconName::Puzzle).text_color(muted).mt_0p5())
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_body(self.touch, cx)
                                    .font_medium()
                                    .child(entry.name.clone()),
                            )
                            .child(source_chip(entry, cx))
                            .children(badges)
                            .when(pending, |row| {
                                row.child(Spinner::new().xsmall().color(muted))
                            }),
                    )
                    .when_some(entry.description.clone(), |col, description| {
                        col.child(div().text_secondary(self.touch, cx).child(description))
                    })
                    .children(components_line(entry, self.touch, cx))
                    .when(entry.source.kind == PluginSourceKind::Command, |col| {
                        col.child(
                            div()
                                .text_secondary(self.touch, cx)
                                .text_color(cx.theme().warning)
                                .child(crate::tr!("providers.plugins.command_source")),
                        )
                    }),
            )
            .children(install.map(|install| div().flex_none().child(install)))
            .into_any_element()
    }
}

/// The install popover's body: the accounts to install to, the scope, and
/// the command each account runs. `marketplace` is where the plugin browsed
/// comes from, named for an account whose own listing does not offer it.
fn install_body(
    panel: &Entity<PluginsSettingsPanel>,
    kind: ProviderKind,
    accounts: &[Account],
    name: &str,
    marketplace: Option<&str>,
    compact: bool,
    cx: &mut App,
) -> AnyElement {
    let this = panel.read(cx);
    let Some(draft) = this.install.as_ref() else {
        return div().into_any_element();
    };
    let project = this.project(cx);
    let entry_id = draft.entry_id.clone();
    let chosen = draft.accounts.clone();
    let scope = draft.scope;
    let muted = cx.theme().muted_foreground;
    // The scopes every ticked account can install to, in the Harness's order.
    let offered: Vec<PluginScope> = kind
        .caps()
        .plugin_management
        .scopes
        .iter()
        .copied()
        .filter(|scope| {
            let mut ticked = accounts
                .iter()
                .filter(|account| chosen.contains(&account.id))
                .peekable();
            ticked.peek().is_some()
                && ticked.all(|account| install_scopes(account, &entry_id).contains(scope))
        })
        .collect();
    let scope = if offered.contains(&scope) {
        scope
    } else {
        offered.first().copied().unwrap_or(scope)
    };
    let project_name = project
        .as_ref()
        .map(|project| project.name.clone())
        .unwrap_or_default();
    let account_rows = accounts.iter().map(|account| {
        let scopes = install_scopes(account, &entry_id);
        let id = account.id.clone();
        let panel = panel.clone();
        let selector = format!("plugin-install-account-{}", account.id);
        let reason = scopes.is_empty().then(|| {
            account
                .user
                .as_ref()
                .and_then(|catalog| catalog.entries.iter().find(|entry| entry.id == entry_id))
                .and_then(|entry| entry.source.marketplace.clone())
                .or_else(|| marketplace.map(str::to_owned))
                .map(|marketplace| {
                    crate::tr!(
                        "providers.plugins.no_marketplace",
                        name = account.name,
                        marketplace = marketplace
                    )
                    .into_owned()
                })
                .unwrap_or_else(|| {
                    crate::tr!("providers.plugins.not_offered", name = account.name).into_owned()
                })
        });
        v_flex()
            .debug_selector(move || selector.clone())
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(material::status_dot(account.color))
                    .child(
                        Checkbox::new(SharedString::from(format!(
                            "plugin-install-account-{}",
                            account.id
                        )))
                        .label(account.name.clone())
                        .checked(chosen.contains(&account.id))
                        .disabled(scopes.is_empty())
                        .on_click(move |checked, _, cx| {
                            let checked = *checked;
                            let id = id.clone();
                            panel.update(cx, |this, cx| {
                                if let Some(draft) = this.install.as_mut() {
                                    if checked {
                                        draft.accounts.insert(id);
                                    } else {
                                        draft.accounts.remove(&id);
                                    }
                                }
                                cx.notify();
                            });
                        }),
                    ),
            )
            .children(reason.map(|reason| {
                div()
                    .pl_5()
                    .text_secondary(compact, cx)
                    .text_color(muted)
                    .child(reason)
            }))
    });
    let scope_label = |scope: PluginScope| match scope {
        PluginScope::Project => crate::tr!(
            "providers.plugins.install_scope.project",
            project = project_name
        )
        .into_owned(),
        PluginScope::Local => crate::tr!(
            "providers.plugins.install_scope.local",
            project = project_name
        )
        .into_owned(),
        _ => crate::tr!("providers.plugins.install_scope.user").into_owned(),
    };
    let scope_help = match scope {
        PluginScope::Project => crate::tr!("providers.plugins.install_scope.project_help"),
        PluginScope::Local => crate::tr!("providers.plugins.install_scope.local_help"),
        _ => crate::tr!("providers.plugins.install_scope.user_help"),
    };
    let scopes = (offered.len() > 1).then(|| {
        let choices = offered.iter().map(|option| {
            let option = *option;
            let panel = panel.clone();
            Choice::new(
                SharedString::from(format!("plugin-install-scope-{}", scope_key(option))),
                scope_label(option),
            )
            .selected(option == scope)
            .on_select(move |_, cx| {
                panel.update(cx, |this, cx| {
                    if let Some(draft) = this.install.as_mut() {
                        draft.scope = option;
                    }
                    cx.notify();
                });
            })
        });
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_caption(cx)
                    .child(crate::tr!("providers.plugins.scope_title")),
            )
            .child(
                ChoiceGroup::new("plugin-install-scopes")
                    .compact(compact)
                    .choices(choices.collect::<Vec<_>>()),
            )
            .child(
                div()
                    .text_secondary(compact, cx)
                    .text_color(muted)
                    .child(scope_help.into_owned()),
            )
    });
    let commands: Vec<AnyElement> = accounts
        .iter()
        .filter(|account| chosen.contains(&account.id))
        .filter_map(|account| {
            let op = PluginOp::Install {
                id: entry_id.clone(),
                scope,
                accept_command: None,
            };
            let line = agent::plugin_op_command(kind, account.binary.as_deref(), &op)?;
            let selector = format!("plugin-install-command-{}", account.id);
            Some(
                v_flex()
                    .debug_selector(move || selector.clone())
                    .gap_0p5()
                    .child(div().text_code(cx).child(verbatim(line, cx)))
                    .child(div().text_secondary(compact, cx).text_color(muted).child(
                        crate::tr!("providers.plugins.runs_as", name = account.name).into_owned(),
                    ))
                    .into_any_element(),
            )
        })
        .collect();
    let count = chosen.len();
    let confirm_panel = panel.clone();
    let cancel_panel = panel.clone();
    v_flex()
        .id("plugin-install-body")
        .map(|col| {
            if compact {
                col.w_full()
            } else {
                col.w(px(380.))
            }
        })
        .p_3()
        .gap_3()
        .when(!compact, |col| {
            col.child(
                div()
                    .text_body(false, cx)
                    .font_medium()
                    .child(crate::tr!("providers.plugins.install_title", name = name).into_owned()),
            )
        })
        .child(
            v_flex()
                .gap_1p5()
                .child(
                    div()
                        .text_caption(cx)
                        .child(crate::tr!("providers.plugins.install_accounts")),
                )
                .children(account_rows),
        )
        .children(scopes)
        .when(!commands.is_empty(), |col| {
            col.child(material::card(cx).w_full().gap_2().children(commands))
        })
        .child(
            h_flex()
                .w_full()
                .justify_end()
                .gap_2()
                .when(!compact, |row| {
                    row.child(
                        Button::new("plugin-install-cancel")
                            .ghost()
                            .label(crate::tr!("settings.cancel").into_owned())
                            .on_click(move |_, _, cx| {
                                cancel_panel.update(cx, |this, cx| {
                                    this.install = None;
                                    cx.notify();
                                })
                            }),
                    )
                })
                .child(
                    Button::new("plugin-install-confirm")
                        .debug_selector(|| "plugin-install-confirm".into())
                        .primary()
                        .when(compact, |button| button.w_full())
                        .disabled(count == 0)
                        .label(if count > 1 {
                            crate::tr!("providers.plugins.install_to_count", count = count)
                                .into_owned()
                        } else {
                            crate::tr!("providers.plugins.action.install").into_owned()
                        })
                        .on_click(move |_, _, cx| {
                            confirm_panel.update(cx, |this, cx| {
                                if let Some(draft) = this.install.as_mut() {
                                    draft.scope = scope;
                                }
                                this.confirm_install(cx)
                            })
                        }),
                ),
        )
        .into_any_element()
}

fn note_row(text: String, cx: &App) -> AnyElement {
    material::group_row_stacked(cx)
        .justify_center()
        .text_secondary(false, cx)
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

/// A card holding one muted line, for a section with nothing to list.
fn note_card(text: String, cx: &App) -> AnyElement {
    material::grouped(vec![note_row(text, cx)], cx).into_any_element()
}

fn source_chip(entry: &ProviderPluginEntry, cx: &App) -> AnyElement {
    let label = match (&entry.source.marketplace, entry.source.kind) {
        (Some(marketplace), PluginSourceKind::Unknown) => marketplace.clone(),
        (Some(marketplace), kind) => format!("{marketplace} · {}", source_kind_label(kind)),
        (None, kind) => source_kind_label(kind),
    };
    material::semantic_chip(label, cx.theme().muted, cx.theme().muted_foreground, cx)
        .into_any_element()
}

/// The counts of what a plugin declares, and what it adds to every session.
fn components_line(entry: &ProviderPluginEntry, compact: bool, cx: &App) -> Option<AnyElement> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(declared) = &entry.declared {
        for (key, names) in [
            ("skills", &declared.skills),
            ("agents", &declared.agents),
            ("hooks", &declared.hooks),
            ("mcp_servers", &declared.mcp_servers),
            ("lsp_servers", &declared.lsp_servers),
            ("apps", &declared.apps),
        ] {
            if let Some(names) = names.as_ref().filter(|names| !names.is_empty()) {
                parts.push(format!(
                    "{} {}",
                    crate::tr!(format!("providers.plugins.declared.{key}")),
                    names.len()
                ));
            }
        }
        if let Some(untrusted) = declared
            .untrusted_hooks
            .as_ref()
            .filter(|hooks| !hooks.is_empty())
        {
            parts.push(format!(
                "{} {}",
                crate::tr!("providers.plugins.declared.untrusted_hooks"),
                untrusted.len()
            ));
        }
    }
    if let Some(cost) = &entry.always_on {
        parts.push(crate::tr!("providers.plugins.always_on", cost = cost).into_owned());
    }
    (!parts.is_empty()).then(|| {
        div()
            .text_secondary(compact, cx)
            .text_color(cx.theme().muted_foreground)
            .child(parts.join(" · "))
            .into_any_element()
    })
}

/// A plugin's details: what it declares by name, and each account's copy.
fn details(
    columns: &[Account],
    row: &Row,
    project: bool,
    scope: PluginScope,
    cx: &App,
) -> AnyElement {
    let entry = &row.entry;
    let muted = cx.theme().muted_foreground;
    let mut col = v_flex().w_full().pt_2().gap_2();
    if let Some(declared) = &entry.declared {
        for (key, names) in [
            ("skills", &declared.skills),
            ("agents", &declared.agents),
            ("hooks", &declared.hooks),
            ("untrusted_hooks", &declared.untrusted_hooks),
            ("mcp_servers", &declared.mcp_servers),
            ("lsp_servers", &declared.lsp_servers),
            ("apps", &declared.apps),
        ] {
            let Some(names) = names else {
                continue;
            };
            col = col.child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2()
                    .text_secondary(false, cx)
                    .child(div().w(px(96.)).flex_none().text_color(muted).child(
                        crate::tr!(format!("providers.plugins.declared.{key}")).into_owned(),
                    ))
                    .child(div().flex_1().min_w_0().child(if names.is_empty() {
                        crate::tr!("providers.plugins.none_declared").into_owned()
                    } else {
                        names.join(", ")
                    })),
            );
        }
    }
    for (account, cell) in columns.iter().zip(&row.cells) {
        let Some(cell) = cell else {
            continue;
        };
        let scopes: Vec<PluginScope> = if project {
            vec![PluginScope::Project, PluginScope::Local]
        } else {
            vec![scope]
        };
        for scope in scopes {
            let Some(installation) = installation(cell, scope) else {
                continue;
            };
            col = col.child(
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .text_secondary(false, cx)
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(material::status_dot(account.color))
                            .child(account.name.clone())
                            .child(div().text_color(muted).child(scope_label(scope)))
                            .children(
                                installation
                                    .version
                                    .clone()
                                    .map(|version| div().text_meta_mono(false, cx).child(version)),
                            ),
                    )
                    .children(installation.location.as_ref().map(|location| {
                        div()
                            .pl_3()
                            .text_meta_mono(false, cx)
                            .child(location.display().to_string())
                    })),
            );
        }
        for (key, text) in &cell.diagnostics {
            col = col.child(
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .child(div().text_caption(cx).child(format!(
                        "{} · {}",
                        diagnostic_label(key),
                        account.name
                    )))
                    .child(
                        material::card(cx)
                            .w_full()
                            .text_code(cx)
                            .child(verbatim(text.clone(), cx)),
                    ),
            );
        }
    }
    col.into_any_element()
}

fn challenge_body(challenge: &PluginChallenge, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let field = |label: SharedString, value: String| {
        v_flex()
            .w_full()
            .gap_0p5()
            .child(div().text_caption(cx).child(label))
            .child(div().w_full().text_code(cx).child(verbatim(value, cx)))
    };
    let body = match &challenge.kind {
        PluginChallengeKind::AcceptCommand {
            command,
            sha256,
            mode,
        } => v_flex()
            .w_full()
            .gap_3()
            .when_some(challenge.native_text.clone(), |col, text| {
                col.child(
                    material::card(cx)
                        .w_full()
                        .text_code(cx)
                        .child(verbatim(text, cx)),
                )
            })
            .child(field(
                crate::tr!("providers.plugins.challenge.command")
                    .into_owned()
                    .into(),
                command.clone(),
            ))
            .child(field(
                crate::tr!("providers.plugins.challenge.sha256")
                    .into_owned()
                    .into(),
                sha256.clone(),
            ))
            .when_some(mode.clone(), |col, mode| {
                col.child(field(
                    crate::tr!("providers.plugins.challenge.mode")
                        .into_owned()
                        .into(),
                    mode,
                ))
            }),
        PluginChallengeKind::ConfirmDestructive { uninstalls } => v_flex()
            .w_full()
            .gap_2()
            .text_body(false, cx)
            .child(if uninstalls.is_empty() {
                crate::tr!("providers.plugins.challenge.destructive_none")
            } else {
                crate::tr!("providers.plugins.challenge.destructive_body")
            })
            .children(uninstalls.iter().map(|id| {
                let selector = format!("plugin-challenge-uninstall-{id}");
                div()
                    .debug_selector(move || selector.clone())
                    .pl_3()
                    .child(verbatim(id.clone(), cx))
            }))
            .when_some(challenge.native_text.clone(), |col, text| {
                col.child(div().text_color(muted).child(verbatim(text, cx)))
            }),
    };
    div()
        .id("plugin-challenge-body")
        .w_full()
        // An alert's description does not shrink with the window, so the body
        // caps itself to keep the alert's actions on screen.
        .max_h(px(360.))
        .overflow_y_scroll_area()
        .child(body)
        .into_any_element()
}

/// Native text exactly as the CLI printed it, in the monospace face.
fn verbatim(text: String, cx: &App) -> AnyElement {
    div()
        .w_full()
        .font_family(cx.theme().mono_font_family.clone())
        .child(text)
        .into_any_element()
}

fn scope_key(scope: PluginScope) -> &'static str {
    match scope {
        PluginScope::User => "user",
        PluginScope::Project => "project",
        PluginScope::Local => "local",
        PluginScope::Managed => "managed",
        PluginScope::Session => "session",
    }
}

fn scope_label(scope: PluginScope) -> String {
    crate::tr!(format!("providers.plugins.scope.{}", scope_key(scope))).into_owned()
}

fn source_kind_label(kind: PluginSourceKind) -> String {
    match kind {
        PluginSourceKind::Official => crate::tr!("providers.plugins.source.official"),
        PluginSourceKind::ThirdParty => crate::tr!("providers.plugins.source.third_party"),
        PluginSourceKind::LocalPath => crate::tr!("providers.plugins.source.local_path"),
        PluginSourceKind::Git => crate::tr!("providers.plugins.source.git"),
        PluginSourceKind::Npm => crate::tr!("providers.plugins.source.npm"),
        PluginSourceKind::Command => crate::tr!("providers.plugins.source.command"),
        PluginSourceKind::Unknown => crate::tr!("providers.plugins.source.unknown"),
    }
    .into_owned()
}

/// Diagnostics are keyed by stable identifiers; one this client does not
/// know yet is shown as its key.
fn diagnostic_label(key: &str) -> String {
    match key {
        "details" | "details_error" | "message" | "update_outcome" | "old_version"
        | "new_version" => crate::tr!(format!("providers.plugins.diagnostic.{key}")).into_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
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

    fn installed(
        mut entry: ProviderPluginEntry,
        scope: PluginScope,
        on: bool,
    ) -> ProviderPluginEntry {
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
        cx.update(crate::theme::init);
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
}
