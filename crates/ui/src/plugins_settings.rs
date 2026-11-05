//! Settings → Plugins: a tab per enabled provider profile whose native
//! plugins Tcode can manage, each with Installed, Available and Marketplaces
//! views of the catalog the host listed for it. Every action, marketplace
//! operation and confirmation comes from the host's catalog; a control the
//! host did not offer is never drawn. A provider whose management is switched
//! off shows only its switch.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use crate::widgets::scrollbar::Scrollbar;
use agent::{
    DeclaredComponents, MarketplaceAction, PluginAction, PluginActionKind, PluginManagement,
    PluginScope, PluginSourceKind, ProviderPluginEntry, Tri,
};
use gpui::{
    AnyElement, App, AppContext as _, Axis, Context, ElementId, Entity, Hsla,
    InteractiveElement as _, IntoElement, ListAlignment, ListState, ParentElement as _, Render,
    ScrollHandle, SharedString, Styled as _, Subscription, Window, div, list, point,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{ScrollableMask, h_flex, v_flex};
use tcode_core::settings::{PluginManagementSettings, ResolvedProfile};
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
use crate::widgets::input::{Input, InputState};
use crate::widgets::switch::Switch;
use crate::widgets::tabs::{TabBar, TabItem};
use crate::widgets::{Collapsible, Spinner};
use crate::window_state::WindowState;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

const SCOPES: [PluginScope; 5] = [
    PluginScope::User,
    PluginScope::Project,
    PluginScope::Local,
    PluginScope::Managed,
    PluginScope::Session,
];

/// The views of one provider's catalog.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Installed,
    Available,
    Marketplaces,
}

impl View {
    fn key(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Available => "available",
            Self::Marketplaces => "marketplaces",
        }
    }
}

pub struct PluginsSettingsPanel {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// The settings page's content scroll, returned to the top when a
    /// provider's tab is opened from elsewhere.
    page_scroll: ScrollHandle,
    /// Whether the page is on screen. Entering it lists every managed
    /// profile again, and challenges are only presented while it shows.
    shown: bool,
    /// The provider tab picked last; the first tab while it is not shown.
    selected: Option<String>,
    view: View,
    /// Filters the Available view.
    search: Entity<InputState>,
    /// Each profile's Available list, so a profile leaving keeps drawing
    /// its own while the next one's comes in.
    lists: std::collections::HashMap<String, AvailableList>,
    /// `(profile id, entry id, catalog index)` rows whose details disclosure is open.
    expanded: HashSet<(String, String, usize)>,
    /// The challenge whose dialog is open.
    challenge: Option<RuntimeOperationId>,
    /// Challenges this client already answered; the replica keeps showing
    /// them until the host's answer arrives.
    answered: HashSet<RuntimeOperationId>,
    /// The switches the shown page last listed for; a provider switched on
    /// since is listed then.
    switches: PluginManagementSettings,
    /// The page's size-class motion: the window draws it under another
    /// parent when it turns compact or wide.
    motion: crate::motion::owned::Owned,
    _subscriptions: Vec<Subscription>,
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
        let subscriptions = vec![
            cx.subscribe_in(
                &store,
                window,
                |this, _, change: &StoreChange, window, cx| match change.topic {
                    TopicKind::Providers | TopicKind::Settings => {
                        if this.shown {
                            this.list_switched_on(cx);
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
            selected: None,
            view: View::Installed,
            search,
            lists: Default::default(),
            expanded: HashSet::new(),
            challenge: None,
            answered: HashSet::new(),
            switches: PluginManagementSettings::default(),
            motion: Default::default(),
            _subscriptions: subscriptions,
        }
    }

    /// The page left the screen; entering it again lists the catalogs anew.
    pub fn hide(&mut self) {
        self.shown = false;
    }

    /// Open `profile_id`'s installed plugins.
    pub fn focus_profile(&mut self, profile_id: String, cx: &mut Context<Self>) {
        self.selected = Some(profile_id);
        self.view = View::Installed;
        self.page_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    /// Enabled profiles whose provider can have its plugins managed.
    fn tab_profiles(&self, cx: &App) -> Vec<ResolvedProfile> {
        self.store
            .read(cx)
            .enabled_profiles()
            .into_iter()
            .filter(|profile| manages(profile.kind.caps().plugin_management))
            .collect()
    }

    /// Profiles Tcode lists plugins for under `switches`.
    fn managed_profiles(&self, switches: &PluginManagementSettings, cx: &App) -> Vec<String> {
        self.tab_profiles(cx)
            .into_iter()
            .filter(|profile| switches.provider_enabled(profile.kind))
            .map(|profile| profile.id)
            .collect()
    }

    fn list_switched_on(&mut self, cx: &mut App) {
        let switches = self.store.read(cx).plugin_management().clone();
        let before = self.managed_profiles(&self.switches, cx);
        for profile_id in self.managed_profiles(&switches, cx) {
            if !before.contains(&profile_id) {
                self.refresh(&profile_id, cx);
            }
        }
        self.switches = switches;
    }

    /// List a profile's catalog from the open thread's directory, or from
    /// outside any project when no thread is open.
    fn refresh(&self, profile_id: &str, cx: &mut App) {
        let profile_id = profile_id.to_string();
        let thread = crate::shell::current_thread(cx);
        self.store.update(cx, |store, cx| {
            let cwd = store.session_cwd_for(thread.as_deref(), cx);
            store.refresh_provider_plugins(profile_id, cwd);
        });
    }

    fn pending_challenges(&self, cx: &App) -> Vec<PluginChallenge> {
        let store = self.store.read(cx);
        store
            .enabled_profiles()
            .iter()
            .filter_map(|profile| store.provider_plugin_catalog(&profile.id))
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

    fn open_add_marketplace(
        &self,
        profile_id: String,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!(
                "providers.plugins.marketplace_source_placeholder"
            ))
        });
        let store = self.store.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let field = input.clone();
            let submit_input = input.clone();
            let store = store.clone();
            let profile_id = profile_id.clone();
            let cwd = cwd.clone();
            dialog
                .wide()
                .title(crate::tr!("providers.plugins.add_marketplace_title").into_owned())
                .content(move |content, _, cx| {
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
                            .child(
                                div()
                                    .text_secondary(false, cx)
                                    .child(crate::tr!("providers.plugins.marketplace_source_help")),
                            ),
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
                                    if source.is_empty() {
                                        return;
                                    }
                                    let (profile_id, cwd) = (profile_id.clone(), cwd.clone());
                                    store.update(cx, |store, _| {
                                        store.add_provider_marketplace(profile_id, source, cwd)
                                    });
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn provider_tabs(
        &self,
        profiles: &[ResolvedProfile],
        selected: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tabs = profiles
            .iter()
            .map(|profile| {
                let name = self
                    .store
                    .read(cx)
                    .provider_profile_display_name(&profile.id);
                let profile_id = profile.id.clone();
                TabItem::new(
                    SharedString::from(format!("plugins-tab-{}", profile.id)),
                    name,
                )
                .icon(crate::provider_card::profile_glyph(
                    self.store.read(cx),
                    profile.kind,
                    Some(&profile.id),
                ))
                .selected(profile.id == selected)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected = Some(profile_id.clone());
                    cx.notify();
                }))
            })
            .collect::<Vec<_>>();
        TabBar::new("plugins-provider-tabs")
            .underline()
            .w_full()
            .aria_label(crate::tr!("settings.plugins"))
            .items(tabs)
            .into_any_element()
    }

    fn render_profile(
        &mut self,
        profile: &ResolvedProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let name = store.provider_profile_display_name(&profile.id);
        let catalog = store.provider_plugin_catalog(&profile.id).cloned();
        let requested_cwd = store.session_cwd_for(crate::shell::current_thread(cx).as_deref(), cx);
        let switches = store.plugin_management();
        let (master, switch_on) = (switches.enabled, switches.provider_switch(profile.kind));
        let muted = cx.theme().muted_foreground;
        let profile_id = profile.id.clone();
        let kind = profile.kind;
        let switch_id = format!("plugins-switch-{profile_id}");
        let switch = div().debug_selector(move || switch_id.clone()).child(
            Switch::new(SharedString::from(format!("plugins-switch-{profile_id}")))
                .checked(switch_on)
                .disabled(!master)
                .tooltip(crate::tr!("providers.plugins.provider_switch", name = name).into_owned())
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    let checked = *checked;
                    this.store.update(cx, |store, _| {
                        store.set_provider_plugin_management(kind, checked)
                    });
                })),
        );

        let off = if !master {
            Some(crate::tr!("providers.plugins.management_off"))
        } else if !switch_on {
            Some(crate::tr!("providers.plugins.provider_off", name = name))
        } else {
            None
        };
        let subtitle = match &off {
            Some(note) => note.clone(),
            None => match catalog
                .as_ref()
                .map_or(requested_cwd, |catalog| catalog.context_cwd.clone())
            {
                Some(path) => {
                    crate::tr!("providers.plugins.project_context", path = path.display())
                }
                None => crate::tr!("providers.plugins.no_project_context"),
            },
        };
        let on = off.is_none();
        let loading = on && catalog.as_ref().is_none_or(|catalog| catalog.loading);
        let refresh_id = format!("plugins-refresh-{profile_id}");
        let spinner = crate::motion::shown(
            SharedString::from(format!("plugins-loading-{profile_id}")),
            loading.then_some(()),
            SHOW,
            window,
            cx,
        );
        let refresh = crate::motion::shown(
            SharedString::from(format!("plugins-refresh-slot-{profile_id}")),
            on.then_some(()),
            SHOW,
            window,
            cx,
        );
        let header = material::group_row(cx)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_body(false, cx)
                            .child(crate::tr!("providers.plugins.provider_switch", name = name)),
                    )
                    .child(div().text_secondary(false, cx).child(subtitle.into_owned())),
            )
            .children(spinner.map(|spinner| spinner.fade(Spinner::new().color(muted))))
            .children(refresh.map(|refresh| {
                refresh.fade(
                    Button::new(SharedString::from(refresh_id.clone()))
                        .debug_selector(move || refresh_id.clone())
                        .ghost()
                        .small()
                        .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                        .tooltip(crate::tr!("providers.plugins.refresh", name = name))
                        .on_click(cx.listener({
                            let profile_id = profile_id.clone();
                            move |this, _, _, cx| this.refresh(&profile_id, cx)
                        })),
                )
            }))
            .child(switch);
        let section_id = format!("plugins-section-{profile_id}");
        let section = v_flex()
            .id(SharedString::from(section_id.clone()))
            .debug_selector(move || section_id.clone())
            .w_full();

        let status = self.status_rows(&profile_id, catalog.as_ref().filter(|_| on), window, cx);
        // Switching a provider off folds its catalog away under the header;
        // a shut fold is not built.
        let body_id = SharedString::from(format!("plugins-body-{profile_id}"));
        let unfolded = gpui_base::motion::transition(
            (body_id.clone(), "built"),
            if on { 1. } else { 0. },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let body = (on || unfolded > 0.).then(|| {
            let listed = crate::motion::CrossFade::new(
                SharedString::from(format!("plugins-listed-{profile_id}")),
                usize::from(catalog.is_some()),
                2,
                CHANGE,
                window,
                cx,
            );
            let mut layers = Vec::new();
            if listed.shows(0) {
                layers.push((
                    0,
                    self.note_card(crate::tr!("providers.plugins.listing"), cx),
                ));
            }
            if let Some(catalog) = catalog.filter(|_| listed.shows(1)) {
                layers.push((1, self.views(profile, &profile_id, catalog, window, cx)));
            }
            listed.render(layers).w_full()
        });
        section
            .child(material::group(cx).child(header).children(status))
            .child(
                Collapsible::new(body_id, on)
                    .motion(&self.motion)
                    .w_full()
                    .content(v_flex().w_full().pt_3().children(body)),
            )
            .into_any_element()
    }

    /// The catalog's views (Installed, Available, Marketplaces) and the one
    /// shown.
    fn views(
        &mut self,
        profile: &ResolvedProfile,
        profile_id: &str,
        catalog: ProviderPluginCatalog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let management = profile.kind.caps().plugin_management;
        let profile_id = profile_id.to_string();
        let installed: Vec<(usize, &ProviderPluginEntry)> = catalog
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.installations.is_empty())
            .collect();
        let available = catalog
            .entries
            .iter()
            .filter(|entry| entry.installations.is_empty())
            .count();
        let mut views = vec![(
            View::Installed,
            crate::tr!("providers.plugins.installed", count = installed.len()),
        )];
        if management.supports(PluginActionKind::Install) {
            views.push((
                View::Available,
                crate::tr!("providers.plugins.available", count = available),
            ));
        }
        if management.marketplaces {
            views.push((
                View::Marketplaces,
                crate::tr!(
                    "providers.plugins.marketplaces",
                    count = catalog.marketplaces.len()
                ),
            ));
        }
        let view = if views.iter().any(|(view, _)| *view == self.view) {
            self.view
        } else {
            View::Installed
        };
        let track = (views.len() > 1).then(|| {
            let panel = cx.entity().downgrade();
            let segments = views.into_iter().map(|(option, label)| {
                let panel = panel.clone();
                TabItem::new(
                    SharedString::from(format!("plugins-view-{}-{profile_id}", option.key())),
                    label.into_owned(),
                )
                .selected(option == view)
                .on_click(move |_, _, cx| {
                    _ = panel.update(cx, |this, cx| {
                        this.view = option;
                        cx.notify();
                    });
                })
            });
            let track_id = format!("plugins-views-{profile_id}");
            div()
                .debug_selector({
                    let track_id = track_id.clone();
                    move || track_id.clone()
                })
                .w(px(440.))
                .max_w_full()
                .child(
                    TabBar::new(SharedString::from(track_id))
                        .segmented()
                        .items(segments),
                )
        });

        let busy = !catalog.pending.is_empty();
        // Each view keeps a layer, so the one left fades out under the one
        // chosen.
        let views = [View::Installed, View::Available, View::Marketplaces];
        let fade = crate::motion::CrossFade::new(
            SharedString::from(format!("plugins-views-body-{profile_id}")),
            views.iter().position(|shown| *shown == view).unwrap_or(0),
            views.len(),
            CHANGE,
            window,
            cx,
        );
        let mut bodies = Vec::new();
        for (layer, shown) in views.into_iter().enumerate() {
            if !fade.shows(layer) {
                continue;
            }
            let body = match shown {
                View::Installed => {
                    let installed = installed.clone();
                    let unlisted = matches!(
                        catalog.state,
                        PluginCatalogState::Error { .. }
                            | PluginCatalogState::Stale {
                                reason: PluginStaleReason::NotLoaded
                            }
                    );
                    // Unlisted shows nothing, so the status row above speaks.
                    let state = crate::motion::CrossFade::new(
                        SharedString::from(format!("plugins-installed-{profile_id}")),
                        match (installed.is_empty(), unlisted) {
                            (false, _) => 2,
                            (true, false) => 1,
                            (true, true) => 0,
                        },
                        3,
                        CHANGE,
                        window,
                        cx,
                    );
                    let mut layers = Vec::new();
                    if state.shows(1) {
                        layers.push((
                            1,
                            self.note_card(crate::tr!("providers.plugins.no_installed"), cx),
                        ));
                    }
                    if state.shows(2) && !installed.is_empty() {
                        let rows = installed.into_iter().map(|(index, entry)| {
                            (SharedString::from(entry.id.clone()), (index, entry.clone()))
                        });
                        let rows = crate::settings_rows::grouped_keyed(
                            SharedString::from(format!("plugins-installed-rows-{profile_id}")),
                            rows,
                            window,
                            cx,
                            |_, (index, entry), window, cx| {
                                self.installed_row(&profile_id, &catalog, *index, entry, window, cx)
                            },
                        );
                        layers.push((2, material::group(cx).child(rows).into_any_element()));
                    }
                    state.render(layers).w_full().into_any_element()
                }
                View::Available => self.available(&profile_id, catalog.clone(), window, cx),
                View::Marketplaces => self.marketplaces(&profile_id, &catalog, busy, window, cx),
            };
            bodies.push((layer, body));
        }
        let content = fade.span().render(bodies).w_full();

        v_flex()
            .w_full()
            .gap_3()
            .children(track)
            .child(content)
            .into_any_element()
    }

    fn status_rows(
        &self,
        profile_id: &str,
        catalog: Option<&ProviderPluginCatalog>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let warning = cx.theme().warning;
        let status = catalog.and_then(|catalog| match &catalog.state {
            PluginCatalogState::Fresh => None,
            PluginCatalogState::Stale { .. } if catalog.loading => Some(CatalogStatus::Note(
                crate::tr!("providers.plugins.listing").into_owned(),
            )),
            PluginCatalogState::Stale { reason } => Some(CatalogStatus::Note(
                match reason {
                    PluginStaleReason::NotLoaded => crate::tr!("providers.plugins.not_listed"),
                    PluginStaleReason::ContextChanged => {
                        crate::tr!("providers.plugins.stale_context")
                    }
                    PluginStaleReason::Changed => crate::tr!("providers.plugins.stale_changed"),
                }
                .into_owned(),
            )),
            PluginCatalogState::Error { message } => Some(CatalogStatus::Failed(message.clone())),
        });
        let mut rows = Vec::new();
        let status_id = SharedString::from(format!("plugins-status-{profile_id}"));
        if let Some(slot) = crate::motion::shown(status_id.clone(), status, SHOW, window, cx) {
            let row = match slot.value.clone() {
                CatalogStatus::Note(text) => {
                    let text = crate::motion::cross_fade(
                        (ElementId::from(status_id), "text"),
                        text,
                        CHANGE,
                        window,
                        cx,
                        |text, _, _| div().child(text.clone()).into_any_element(),
                    );
                    material::group_row_stacked(cx)
                        .justify_center()
                        .text_secondary(false, cx)
                        .text_color(muted)
                        .child(text)
                }
                CatalogStatus::Failed(message) => material::group_row_stacked(cx)
                    .debug_selector(|| "plugins-list-error".into())
                    .gap_1()
                    .text_secondary(false, cx)
                    .text_color(danger)
                    .child(crate::tr!("providers.plugins.list_failed").into_owned())
                    .child(verbatim(message, cx)),
            };
            rows.push(slot.fold(crate::settings_rows::hairlined(row, cx)));
        }
        // Each line under the header draws the hairline above it, so the
        // line comes and goes with it.
        let errors = text_keys(catalog.iter().flat_map(|catalog| &catalog.errors));
        let errors = crate::motion::keyed(
            SharedString::from(format!("plugins-catalog-errors-{profile_id}")),
            errors,
            ROWS,
            window,
            cx,
            |_, error, _, cx| {
                crate::settings_rows::hairlined(
                    material::group_row_stacked(cx)
                        .text_secondary(false, cx)
                        .text_color(warning)
                        .child(verbatim(error.clone(), cx)),
                    cx,
                )
            },
        );
        rows.push(errors.render(v_flex().w_full()));
        rows
    }

    fn installed_row(
        &self,
        profile_id: &str,
        catalog: &ProviderPluginCatalog,
        index: usize,
        entry: &ProviderPluginEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let busy = !catalog.pending.is_empty();
        let pending = catalog.pending.contains(&entry.id);
        let key = (profile_id.to_string(), entry.id.clone(), index);
        let expanded = self.expanded.contains(&key);
        let has_details = entry.declared.is_some()
            || !entry.diagnostics.is_empty()
            || entry
                .installations
                .iter()
                .any(|installation| installation.location.is_some());
        let (enabled_label, enabled_bg, enabled_fg) = match entry.enabled {
            Tri::Yes => (
                crate::tr!("providers.plugins.effective_enabled"),
                cx.theme().success.opacity(0.12),
                cx.theme().success_foreground,
            ),
            Tri::No => (
                crate::tr!("providers.plugins.effective_disabled"),
                cx.theme().muted,
                muted,
            ),
            Tri::Unknown => (
                crate::tr!("providers.plugins.effective_unknown"),
                cx.theme().muted,
                muted,
            ),
        };
        let row_id = format!("plugin-row-{profile_id}-{}", entry.id);
        let toggle_id = format!("plugin-details-{profile_id}-{}", entry.id);
        let settle = || gpui_base::motion::Transition::new(GLIDE);
        let enabled_id = SharedString::from(format!("{row_id}-enabled"));
        let enabled_bg = gpui_base::motion::transition(
            (enabled_id.clone(), "bg"),
            enabled_bg,
            settle(),
            window,
            cx,
        );
        let enabled_fg = gpui_base::motion::transition(
            (enabled_id.clone(), "fg"),
            enabled_fg,
            settle(),
            window,
            cx,
        );
        let enabled_label = crate::motion::cross_fade(
            (ElementId::from(enabled_id), "label"),
            SharedString::from(enabled_label.into_owned()),
            CHANGE,
            window,
            cx,
            |label, _, _| label.clone().into_any_element(),
        );
        let spinner = crate::motion::shown(
            SharedString::from(format!("{row_id}-pending")),
            pending.then_some(()),
            SHOW,
            window,
            cx,
        );
        let turn = gpui_base::motion::transition(
            (SharedString::from(toggle_id.clone()), "turn"),
            if expanded { 1. } else { 0. },
            settle(),
            window,
            cx,
        );
        let title = h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(div().text_body(false, cx).child(entry.name.clone()))
                    .when_some(entry.version.clone(), |row, version| {
                        row.child(div().text_meta_mono(false, cx).child(version))
                    })
                    .child(
                        material::chip_surface(enabled_bg, enabled_fg, cx)
                            .overflow_hidden()
                            .child(enabled_label),
                    )
                    .children(spinner.map(|spinner| spinner.fade(Spinner::new().color(muted)))),
            )
            .when(has_details, |row| {
                row.child(
                    Button::new(SharedString::from(toggle_id.clone()))
                        .debug_selector(move || toggle_id.clone())
                        .ghost()
                        .small()
                        .icon(
                            Icon::new(IconName::ChevronDown)
                                .rotate(gpui::radians(std::f32::consts::PI * turn)),
                        )
                        .label(crate::tr!("providers.plugins.details").into_owned())
                        .tooltip(crate::tr!(
                            "providers.plugins.toggle_details",
                            name = entry.name
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded.remove(&key) {
                                this.expanded.insert(key.clone());
                            }
                            cx.notify();
                        })),
                )
            });

        let scopes: Vec<(SharedString, PluginScope)> = SCOPES
            .into_iter()
            .filter(|scope| {
                entry
                    .installations
                    .iter()
                    .any(|installation| installation.scope == *scope)
                    || entry.actions.iter().any(|action| action.scope() == *scope)
            })
            .map(|scope| (SharedString::from(scope_key(scope)), scope))
            .collect();
        let any_scope = !scopes.is_empty();
        // A scope's line, and an action on it, comes and goes as an install,
        // an enable or a removal changes what the entry has where.
        let scope_lines = crate::motion::keyed(
            SharedString::from(format!("{row_id}-scopes")),
            scopes,
            ROWS,
            window,
            cx,
            |scope_id, scope, window, cx| {
                let scope = *scope;
                let installation = entry
                    .installations
                    .iter()
                    .find(|installation| installation.scope == scope);
                let facts = h_flex()
                    .flex_none()
                    .items_center()
                    .gap_x_2()
                    .text_color(muted)
                    .map(|row| match installation {
                        None => row.child(crate::tr!("providers.plugins.not_installed_in_scope")),
                        Some(installation) => row
                            .when_some(installation.version.clone(), |row, version| {
                                row.child(
                                    div()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .child(version),
                                )
                            })
                            .when_some(installation.scope_enabled, |row, enabled| {
                                row.child(if enabled {
                                    crate::tr!("providers.plugins.scope_enabled")
                                } else {
                                    crate::tr!("providers.plugins.scope_disabled")
                                })
                            }),
                    });
                let actions: Vec<(SharedString, PluginAction)> = ordered(
                    entry
                        .actions
                        .iter()
                        .copied()
                        .filter(|action| action.scope() == scope)
                        .collect(),
                )
                .into_iter()
                .map(|action| (SharedString::from(format!("{action:?}")), action))
                .collect();
                // Each button carries the gap before it, so the gap goes
                // with the button.
                let actions = crate::motion::keyed_across(
                    SharedString::from(format!("{row_id}-{scope_id}-actions")),
                    actions,
                    ROWS,
                    window,
                    cx,
                    |_, action, _, _| {
                        div()
                            .pl_1()
                            .child(action_button(
                                &self.store,
                                profile_id,
                                &entry.id,
                                *action,
                                catalog.context_cwd.clone(),
                                busy,
                            ))
                            .into_any_element()
                    },
                );
                div()
                    .w_full()
                    .pt(cx.theme().tokens.spacing.sm)
                    .child(
                        h_flex()
                            .w_full()
                            .min_h(px(material::bar_height(material::Bar::Section, false)))
                            .flex_wrap()
                            .items_center()
                            .gap_x_3()
                            .gap_y_1()
                            .text_token(&cx.theme().tokens.typography.xs)
                            .child(
                                div()
                                    .flex_none()
                                    // One column, so the scopes' facts line up.
                                    .w(px(72.))
                                    .text_caption(cx)
                                    .child(scope_label(scope)),
                            )
                            .child(facts)
                            .child(actions.render(h_flex().ml_auto())),
                    )
                    .into_any_element()
            },
        );

        let errors = crate::motion::keyed(
            SharedString::from(format!("{row_id}-errors")),
            text_keys(&entry.errors),
            ROWS,
            window,
            cx,
            |_, error, _, cx| {
                div()
                    .pt_1p5()
                    .text_secondary(false, cx)
                    .text_color(cx.theme().danger)
                    .child(verbatim(error.clone(), cx))
                    .into_any_element()
            },
        );
        material::group_row_stacked(cx)
            .id((SharedString::from(row_id.clone()), index))
            .debug_selector(move || row_id.clone())
            .gap_1p5()
            .child(title)
            .when_some(entry.description.clone(), |col, description| {
                col.child(div().text_secondary(false, cx).child(description))
            })
            .child(source_line(entry, cx))
            .when(any_scope, |col| {
                // Each line carries the gap above it, so it goes with it.
                col.child(scope_lines.render(material::card(cx).w_full().gap_0().pt_0()))
            })
            // Each error carries the gap above it; the list gives back the
            // gap the column puts before it, so no errors take no room.
            .child(errors.render(v_flex().w_full().mt(gpui::rems(-0.375))))
            .child(
                Collapsible::new("plugin-details", expanded)
                    .motion(&self.motion)
                    .w_full()
                    .content(details(entry, cx)),
            )
            .into_any_element()
    }

    /// The plugins the catalog offers but has not installed, filtered by the
    /// search field.
    fn available(
        &mut self,
        profile_id: &str,
        catalog: ProviderPluginCatalog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let none_available = !catalog
            .entries
            .iter()
            .any(|entry| entry.installations.is_empty());
        let matches: Vec<usize> = catalog
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.installations.is_empty()
                    && (query.is_empty()
                        || entry.name.to_lowercase().contains(&query)
                        || entry
                            .description
                            .as_deref()
                            .is_some_and(|text| text.to_lowercase().contains(&query)))
            })
            .map(|(index, _)| index)
            .collect();
        let listed = matches
            .iter()
            .map(|&index| (index, catalog.entries[index].id.clone()))
            .collect::<Vec<_>>();
        let row_key = |id: &str| {
            use std::hash::{Hash as _, Hasher as _};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            id.hash(&mut hasher);
            hasher.finish()
        };
        let keys: Rc<[u64]> = listed.iter().map(|(_, id)| row_key(id)).collect();
        let available = self.lists.entry(profile_id.to_string()).or_default();
        if available.listed.as_ref() != Some(&listed) {
            // A keystroke or an install folds rows in and out and glides the
            // rest; the list first shown, or shown after none, is in place.
            if let Some(old) = available.listed.as_ref().filter(|old| !old.is_empty()) {
                let old_keys: Vec<u64> = old.iter().map(|(_, id)| row_key(id)).collect();
                let old_indices: Vec<usize> = old.iter().map(|(index, _)| *index).collect();
                available.flip.replace(
                    &available.list,
                    |index| old_keys.get(index).copied(),
                    &keys,
                    window,
                );
                available.rows.replace(
                    &old_keys,
                    &keys,
                    |_, _| Some(()),
                    |index| old_indices.get(index).copied(),
                    cx,
                );
            }
            available.list.reset(matches.len());
            available.listed = Some(listed);
        }
        available
            .rows
            .settle_list(&available.list, &keys, window, cx);
        let list_state = available.list.clone();
        // Listing cross-fades to what it found; which of those shows follows
        // the search keystrokes and does not.
        let listing = matches.is_empty() && catalog.loading && none_available;
        let state = crate::motion::CrossFade::new(
            SharedString::from(format!("plugins-available-{profile_id}")),
            usize::from(!listing),
            2,
            CHANGE,
            window,
            cx,
        )
        .fill();
        let mut layers = Vec::new();
        if state.shows(0) {
            layers.push((
                0,
                self.note_row(
                    crate::tr!("providers.plugins.listing").into_owned(),
                    cx.theme().muted_foreground,
                    cx,
                ),
            ));
        }
        let results = state.shows(1).then(|| {
            if !matches.is_empty() {
                let catalog = Rc::new(catalog);
                let profile_id = profile_id.to_string();
                crate::scroll::page_viewport(
                    "plugins-available-bounce",
                    &list_state,
                    list(
                        list_state.clone(),
                        cx.processor(move |this, ix: usize, window, cx| {
                            let index = matches[ix];
                            let row = this.available_row(
                                &profile_id,
                                &catalog,
                                index,
                                &catalog.entries[index],
                                window,
                                cx,
                            );
                            let row =
                                v_flex()
                                    .w_full()
                                    .child(row)
                                    .when(ix + 1 != matches.len(), |col| {
                                        col.child(material::row_hairline(
                                            px(material::CARD_INSET),
                                            cx,
                                        ))
                                    });
                            let key = keys[ix];
                            let Some(available) = this.lists.get(&profile_id) else {
                                return row.into_any_element();
                            };
                            let around = available.rows.around(key, cx);
                            let mut left: std::collections::HashMap<usize, AnyElement> = around
                                .above
                                .iter()
                                .chain(&around.below)
                                .filter_map(|(_, index, _)| {
                                    let entry = catalog.entries.get(*index)?;
                                    let row = this.available_row(
                                        &profile_id,
                                        &catalog,
                                        *index,
                                        entry,
                                        window,
                                        cx,
                                    );
                                    Some((*index, row))
                                })
                                .collect();
                            let row = around.draw(
                                "plugins-available-row",
                                key,
                                crate::motion::list_rows::Glide::Rest,
                                row,
                                |index, _| {
                                    left.remove(index)
                                        .unwrap_or_else(|| gpui::Empty.into_any_element())
                                },
                                window,
                            );
                            match this.lists.get(&profile_id) {
                                Some(available) => {
                                    available.flip.row(ix, ("plugins-available-row", key), row)
                                }
                                None => row,
                            }
                        }),
                    )
                    .size_full(),
                )
                .into_any_element()
            } else if none_available {
                self.note_row(
                    crate::tr!("providers.plugins.no_available").into_owned(),
                    cx.theme().muted_foreground,
                    cx,
                )
            } else {
                self.note_row(
                    crate::tr!("providers.plugins.no_matches").into_owned(),
                    cx.theme().muted_foreground,
                    cx,
                )
            }
        });
        layers.extend(results.map(|results| (1, results)));
        let body = state.render(layers).size_full();
        v_flex()
            .w_full()
            .gap_2()
            .when(!none_available, |col| {
                col.child(
                    Input::new(&self.search).placeholder(crate::tr!("providers.plugins.search")),
                )
            })
            .child(
                material::group(cx)
                    .h(px(crate::overlay::DIALOG_LIST_HEIGHT))
                    .relative()
                    .child(body)
                    .child(crate::scroll::list_height_hint(&list_state, px(100.)))
                    .child(
                        ScrollableMask::new(Axis::Vertical, &list_state)
                            .id("plugins-available-mask"),
                    )
                    .when(!none_available && !window.is_inspector_picking(cx), |col| {
                        col.child(
                            Scrollbar::vertical(&list_state)
                                .id("plugins-available-scrollbar")
                                .motion(&self.motion),
                        )
                    }),
            )
            .into_any_element()
    }

    fn available_row(
        &self,
        profile_id: &str,
        catalog: &ProviderPluginCatalog,
        index: usize,
        entry: &ProviderPluginEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let busy = !catalog.pending.is_empty();
        let pending = catalog.pending.contains(&entry.id);
        let row_id = format!("plugin-available-{profile_id}-{}", entry.id);
        let spinner = crate::motion::shown(
            SharedString::from(format!("{row_id}-pending")),
            pending.then_some(()),
            SHOW,
            window,
            cx,
        );
        let installs: Vec<PluginAction> = entry
            .actions
            .iter()
            .copied()
            .filter(|action| action.kind() == PluginActionKind::Install)
            .collect();
        let button = |action: PluginAction| {
            action_button(
                &self.store,
                profile_id,
                &entry.id,
                action,
                catalog.context_cwd.clone(),
                busy,
            )
        };
        let install = match installs.as_slice() {
            [] => None,
            [only] => Some(button(*only).into_any_element()),
            several => Some(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .justify_end()
                    .gap_1()
                    .child(
                        div()
                            .text_secondary(false, cx)
                            .child(crate::tr!("providers.plugins.install_to")),
                    )
                    .children(
                        several
                            .iter()
                            .map(|action| button(*action).label(scope_label(action.scope()))),
                    )
                    .into_any_element(),
            ),
        };
        material::group_row_stacked(cx)
            .id((SharedString::from(row_id.clone()), index))
            .debug_selector(move || row_id.clone())
            .flex_row()
            .items_start()
            .gap_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(div().text_body(false, cx).child(entry.name.clone()))
                            .when_some(entry.version.clone(), |row, version| {
                                row.child(div().text_meta_mono(false, cx).child(version))
                            })
                            .children(
                                spinner.map(|spinner| spinner.fade(Spinner::new().color(muted))),
                            ),
                    )
                    .when_some(entry.description.clone(), |col, description| {
                        col.child(div().text_secondary(false, cx).child(description))
                    })
                    .child(source_line(entry, cx))
                    .when(entry.source.kind == PluginSourceKind::Command, |col| {
                        col.child(
                            div()
                                .text_secondary(false, cx)
                                .text_color(cx.theme().warning)
                                .child(crate::tr!("providers.plugins.command_source")),
                        )
                    })
                    .when_some(entry.declared.as_ref(), |col, declared| {
                        col.child(declared_chips(declared, cx))
                    })
                    .children(entry.errors.iter().map(|error| {
                        div()
                            .text_secondary(false, cx)
                            .text_color(cx.theme().danger)
                            .child(verbatim(error.clone(), cx))
                    })),
            )
            .children(install.map(|install| div().flex_none().max_w(px(240.)).child(install)))
            .into_any_element()
    }

    fn marketplaces(
        &self,
        profile_id: &str,
        catalog: &ProviderPluginCatalog,
        busy: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let listed = !catalog.marketplaces.is_empty();
        let rows = catalog.marketplaces.iter().map(|marketplace| {
            (
                SharedString::from(marketplace.name.clone()),
                marketplace.clone(),
            )
        });
        let rows = crate::settings_rows::grouped_keyed(
            SharedString::from(format!("plugin-marketplace-rows-{profile_id}")),
            rows,
            window,
            cx,
            |_, marketplace, window, cx| {
                let removable = catalog.marketplace_actions.iter().any(|action| {
                    matches!(action, MarketplaceAction::Remove { marketplace: name, .. } if *name == marketplace.name)
                });
                let spinner = crate::motion::shown(
                    SharedString::from(format!(
                        "plugin-marketplace-pending-{profile_id}-{}",
                        marketplace.name
                    )),
                    catalog.pending.contains(&marketplace.name).then_some(()),
                    SHOW,
                    window,
                    cx,
                );
                let remove_id = format!(
                    "plugin-marketplace-remove-{profile_id}-{}",
                    marketplace.name
                );
                let location = marketplace
                    .location
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .filter(|location| *location != marketplace.source);
                material::group_row_stacked(cx)
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div().text_body(false, cx).child(marketplace.name.clone()),
                                    )
                                    .child(material::semantic_chip(
                                        source_kind_label(marketplace.kind),
                                        cx.theme().muted,
                                        muted,
                                        cx,
                                    ))
                                    .children(
                                        spinner.map(|spinner| {
                                            spinner.fade(Spinner::new().color(muted))
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .text_meta_mono(false, cx)
                                    .child(marketplace.source.clone()),
                            )
                            .when_some(location, |col, location| {
                                col.child(div().text_meta_mono(false, cx).child(location))
                            }),
                    )
                    .when(removable, |row| {
                        let name = marketplace.name.clone();
                        let profile_id = profile_id.to_string();
                        let cwd = catalog.context_cwd.clone();
                        let store = self.store.clone();
                        row.child(
                            Button::new(SharedString::from(remove_id.clone()))
                                .debug_selector(move || remove_id.clone())
                                .outline()
                                .danger()
                                .small()
                                .disabled(busy)
                                .label(crate::tr!("providers.plugins.remove").into_owned())
                                .on_click(move |_, _, cx| {
                                    let (profile_id, name, cwd) =
                                        (profile_id.clone(), name.clone(), cwd.clone());
                                    store.update(cx, |store, _| {
                                        store.remove_provider_marketplace(profile_id, name, cwd)
                                    });
                                }),
                        )
                    })
                    .into_any_element()
            },
        );
        let state = crate::motion::CrossFade::new(
            SharedString::from(format!("plugin-marketplaces-{profile_id}")),
            usize::from(listed),
            2,
            CHANGE,
            window,
            cx,
        );
        let mut layers = Vec::new();
        if state.shows(0) {
            layers.push((
                0,
                self.note_card(crate::tr!("providers.plugins.no_marketplaces"), cx),
            ));
        }
        if state.shows(1) && listed {
            layers.push((1, material::group(cx).child(rows).into_any_element()));
        }
        let list = state.render(layers).w_full();
        let can_add = catalog
            .marketplace_actions
            .contains(&MarketplaceAction::Add);
        let add_id = format!("plugin-marketplace-add-{profile_id}");
        v_flex()
            .w_full()
            .gap_2()
            .when(can_add, |col| {
                col.child(
                    h_flex().w_full().justify_end().child(
                        Button::new(SharedString::from(add_id.clone()))
                            .debug_selector(move || add_id.clone())
                            .outline()
                            .small()
                            .icon(IconName::Plus)
                            .disabled(busy)
                            .label(crate::tr!("providers.plugins.add_marketplace").into_owned())
                            .on_click(cx.listener({
                                let profile_id = profile_id.to_string();
                                let cwd = catalog.context_cwd.clone();
                                move |this, _, window, cx| {
                                    this.open_add_marketplace(
                                        profile_id.clone(),
                                        cwd.clone(),
                                        window,
                                        cx,
                                    )
                                }
                            })),
                    ),
                )
            })
            .child(list)
            .into_any_element()
    }

    fn master_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let enabled = self.store.read(cx).plugin_management().enabled;
        material::group(cx)
            .child(
                material::group_row(cx)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_body(false, cx)
                            .child(crate::tr!("providers.plugins.management")),
                    )
                    .child(
                        div()
                            .debug_selector(|| "plugins-management-switch".into())
                            .child(
                                Switch::new("plugins-management-switch")
                                    .checked(enabled)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        let checked = *checked;
                                        this.store.update(cx, |store, _| {
                                            store.set_plugin_management_enabled(checked)
                                        });
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn note_row(&self, text: String, color: Hsla, cx: &Context<Self>) -> AnyElement {
        material::group_row_stacked(cx)
            .justify_center()
            .text_secondary(false, cx)
            .text_color(color)
            .child(text)
            .into_any_element()
    }

    /// A card holding one muted line, for a view with nothing to list.
    fn note_card(&self, text: std::borrow::Cow<'static, str>, cx: &Context<Self>) -> AnyElement {
        material::grouped(
            vec![self.note_row(text.into_owned(), cx.theme().muted_foreground, cx)],
            cx,
        )
        .into_any_element()
    }
}

/// One profile's Available list: its rows as last listed, by catalog index
/// and entry id, and the rows a search or an install brings in and takes
/// away, drawn from their catalog index as they fold.
struct AvailableList {
    list: ListState,
    listed: Option<Vec<(usize, String)>>,
    rows: crate::motion::list_rows::ListRows<(), usize>,
    flip: crate::motion::ListFlip,
}

impl Default for AvailableList {
    fn default() -> Self {
        Self {
            list: ListState::new(0, ListAlignment::Top, px(120.)),
            listed: None,
            rows: crate::motion::list_rows::ListRows::over(crate::motion::KEYSTROKE),
            flip: Default::default(),
        }
    }
}

impl Render for PluginsSettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.shown && self.store.read(cx).settings_hydrated() {
            self.shown = true;
            self.switches = self.store.read(cx).plugin_management().clone();
            for profile_id in self.managed_profiles(&self.switches, cx) {
                self.refresh(&profile_id, cx);
            }
            cx.defer_in(window, |this, window, cx| this.sync_challenge(window, cx));
        }
        let profiles = self.tab_profiles(cx);
        let selected = profiles
            .iter()
            .find(|profile| self.selected.as_ref() == Some(&profile.id))
            .or(profiles.first())
            .cloned();
        let compact = self.window_state.read(cx).compact;
        let gap = self.motion.transition(
            "page-gap",
            if compact {
                cx.theme().tokens.spacing.lg
            } else {
                material::section_gap(cx)
            },
            GLIDE,
            window,
            cx,
        );
        v_flex()
            .w_full()
            .gap(gap)
            .child(material::group_caption(
                crate::tr!("settings.plugins_section"),
                cx,
            ))
            .child(self.master_switch(cx))
            .map(|page| match selected {
                None => {
                    page.child(self.note_card(crate::tr!("providers.plugins.no_providers"), cx))
                }
                Some(profile) => {
                    // Each provider's body keeps a layer, so the one left
                    // fades out under the one chosen.
                    let current = profiles
                        .iter()
                        .position(|shown| shown.id == profile.id)
                        .unwrap_or(0);
                    let fade = crate::motion::CrossFade::new(
                        "plugins-profile-body",
                        current,
                        profiles.len(),
                        CHANGE,
                        window,
                        cx,
                    );
                    let bodies: Vec<(usize, AnyElement)> = profiles
                        .iter()
                        .enumerate()
                        .filter(|(layer, _)| fade.shows(*layer))
                        .map(|(layer, shown)| (layer, self.render_profile(shown, window, cx)))
                        .collect();
                    page.child(
                        v_flex()
                            .w_full()
                            .gap_3()
                            .child(self.provider_tabs(&profiles, &profile.id, cx))
                            .child(fade.span().render(bodies).w_full()),
                    )
                }
            })
    }
}

/// Keys for lines that are only their text, such as errors: a line keeps its
/// key while it stays, and two alike are told apart by their order.
fn text_keys<'a>(lines: impl IntoIterator<Item = &'a String>) -> Vec<(SharedString, String)> {
    let mut seen = std::collections::HashMap::<&str, usize>::new();
    lines
        .into_iter()
        .map(|line| {
            let count = seen.entry(line.as_str()).or_default();
            *count += 1;
            (SharedString::from(format!("{count}-{line}")), line.clone())
        })
        .collect()
}

fn manages(management: PluginManagement) -> bool {
    !management.actions.is_empty() || management.marketplaces
}

/// What the catalog's status row says, if it shows one.
#[derive(Clone, PartialEq)]
enum CatalogStatus {
    Note(String),
    Failed(String),
}

/// Enable and Disable side by side first, then Update, then Uninstall.
fn ordered(mut actions: Vec<PluginAction>) -> Vec<PluginAction> {
    actions.sort_by_key(|action| match action.kind() {
        PluginActionKind::Install => 0,
        PluginActionKind::Enable => 1,
        PluginActionKind::Disable => 2,
        PluginActionKind::Update => 3,
        PluginActionKind::Uninstall => 4,
    });
    actions
}

fn action_button(
    store: &Entity<WorkspaceStore>,
    profile_id: &str,
    entry_id: &str,
    action: PluginAction,
    cwd: Option<PathBuf>,
    busy: bool,
) -> Button {
    let (verb, label) = match action.kind() {
        PluginActionKind::Install => ("install", crate::tr!("providers.plugins.action.install")),
        PluginActionKind::Uninstall => (
            "uninstall",
            crate::tr!("providers.plugins.action.uninstall"),
        ),
        PluginActionKind::Enable => ("enable", crate::tr!("providers.plugins.action.enable")),
        PluginActionKind::Disable => ("disable", crate::tr!("providers.plugins.action.disable")),
        PluginActionKind::Update => ("update", crate::tr!("providers.plugins.action.update")),
    };
    let id = format!(
        "plugin-{verb}-{profile_id}-{entry_id}-{}",
        scope_key(action.scope())
    );
    let store = store.clone();
    let (profile_id, entry_id) = (profile_id.to_string(), entry_id.to_string());
    let button = Button::new(SharedString::from(id.clone()))
        .debug_selector(move || id.clone())
        .outline()
        .small()
        .disabled(busy)
        .label(label.into_owned())
        .on_click(move |_, _, cx| {
            let (profile_id, entry_id, cwd) = (profile_id.clone(), entry_id.clone(), cwd.clone());
            store.update(cx, |store, _| {
                store.run_provider_plugin_action(profile_id, entry_id, action, cwd)
            });
        });
    if action.kind() == PluginActionKind::Uninstall {
        button.danger()
    } else {
        button
    }
}

fn source_line(entry: &ProviderPluginEntry, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap_2()
        .text_secondary(false, cx)
        .when_some(entry.source.marketplace.as_ref(), |row, marketplace| {
            row.child(
                crate::tr!(
                    "providers.plugins.from_marketplace",
                    marketplace = marketplace
                )
                .into_owned(),
            )
        })
        .child(material::semantic_chip(
            source_kind_label(entry.source.kind),
            cx.theme().muted,
            muted,
            cx,
        ))
        .into_any_element()
}

/// Where each installation lives, the declared components and the host's
/// read-only facts for an entry.
fn details(entry: &ProviderPluginEntry, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .pt_1()
        .gap_2()
        .children(entry.installations.iter().filter_map(|installation| {
            let location = installation.location.as_ref()?;
            Some(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2()
                    .text_secondary(false, cx)
                    .child(
                        div()
                            .flex_none()
                            .text_caption(cx)
                            .child(scope_label(installation.scope)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(location.display().to_string()),
                    ),
            )
        }))
        .when_some(entry.declared.as_ref(), |col, declared| {
            col.child(declared_chips(declared, cx))
        })
        .children(entry.diagnostics.iter().map(|(key, text)| {
            v_flex()
                .w_full()
                .gap_0p5()
                .child(div().text_caption(cx).child(diagnostic_label(key)))
                .child(
                    material::card(cx)
                        .w_full()
                        .text_code(cx)
                        .child(verbatim(text.clone(), cx)),
                )
        }))
        .into_any_element()
}

fn declared_chips(declared: &DeclaredComponents, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let groups = [
        ("skills", &declared.skills, false),
        ("agents", &declared.agents, false),
        ("hooks", &declared.hooks, false),
        ("untrusted_hooks", &declared.untrusted_hooks, true),
        ("mcp_servers", &declared.mcp_servers, false),
        ("lsp_servers", &declared.lsp_servers, false),
        ("apps", &declared.apps, false),
    ];
    v_flex()
        .w_full()
        .gap_1()
        .children(groups.into_iter().filter_map(|(key, names, warn)| {
            let names = names.as_ref()?;
            let (bg, fg) = if warn {
                (
                    cx.theme().warning.opacity(0.14),
                    cx.theme().warning_foreground,
                )
            } else {
                (cx.theme().muted, muted)
            };
            Some(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(div().text_caption(cx).child(format!(
                        "{} · {}",
                        crate::tr!(format!("providers.plugins.declared.{key}")),
                        names.len()
                    )))
                    .when(names.is_empty(), |row| {
                        row.child(
                            div()
                                .text_secondary(false, cx)
                                .child(crate::tr!("providers.plugins.none_declared")),
                        )
                    })
                    .children(
                        names
                            .iter()
                            .map(|name| material::semantic_chip(name.clone(), bg, fg, cx)),
                    ),
            )
        }))
        .into_any_element()
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
    use tcode_core::settings::Settings;
    use tcode_protocol::{
        ClientPayload, Command, CommandResponse, EventEnvelope, HostMessage, ProvidersStatus,
        ServerEvent, SettingsPatch, Topic, decode_client_line, encode_line,
    };

    use super::*;
    use crate::settings_page::SettingsPage;
    use crate::store::WorkspaceAttachment;

    struct Host {
        outgoing: async_channel::Receiver<String>,
        incoming: async_channel::Sender<String>,
        store: Entity<WorkspaceStore>,
        _pump: gpui::Task<()>,
    }

    impl Host {
        fn replicate(&self, catalog: ProviderPluginCatalog, cx: &mut VisualTestContext) {
            send_providers(&self.incoming, catalog);
            self.settle(cx);
        }

        fn replicate_settings(&self, settings: Settings, cx: &mut VisualTestContext) {
            send_settings(&self.incoming, settings);
            self.settle(cx);
        }

        fn settle(&self, cx: &mut VisualTestContext) {
            cx.run_until_parked();
            self.store
                .update(cx, |store, cx| store.drain_host_events_for_test(cx));
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

    fn catalog(
        entries: Vec<ProviderPluginEntry>,
        challenges: Vec<PluginChallenge>,
    ) -> ProviderPluginCatalog {
        ProviderPluginCatalog {
            profile_id: "claude".into(),
            context_cwd: Some("/work/project".into()),
            marketplaces: Vec::new(),
            marketplace_actions: Vec::new(),
            entries,
            errors: Vec::new(),
            state: PluginCatalogState::Fresh,
            loading: false,
            pending: Vec::new(),
            challenges,
        }
    }

    /// Settings with Claude Code's plugin management switched on.
    fn claude_managed() -> Settings {
        let mut settings = Settings::default();
        settings
            .apply(SettingsPatch::PluginManagementProvider {
                provider: agent::ProviderKind::ClaudeCode,
                enabled: true,
            })
            .unwrap();
        settings
    }

    /// Open Settings → Plugins the way navigation does, against a host
    /// whose replicated state holds `settings` and `catalog`.
    fn open_plugins(
        settings: Settings,
        catalog: ProviderPluginCatalog,
        cx: &mut TestAppContext,
    ) -> (Host, &mut VisualTestContext) {
        let host = plugins_host(settings, catalog, cx);
        let window_state = cx.new(|_| {
            let mut state = WindowState::new(false);
            state.pending_settings_section = Some("plugins".into());
            state
        });
        let page_store = host.store.clone();
        let (_, cx) = cx.add_window_view(|window, cx| {
            let page = cx.new(|cx| SettingsPage::new(page_store, window_state, window, cx));
            gpui_base::Root::new(page, window, cx)
        });
        cx.simulate_resize(size(px(1000.), px(1400.)));
        cx.run_until_parked();
        host.store
            .update(cx, |store, cx| store.drain_host_events_for_test(cx));
        draw(cx);
        (host, cx)
    }

    /// A host whose replicated state holds `settings` and `catalog`.
    fn plugins_host(
        settings: Settings,
        catalog: ProviderPluginCatalog,
        cx: &mut TestAppContext,
    ) -> Host {
        cx.update(crate::theme::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        send_settings(&incoming, settings);
        send_providers(&incoming, catalog);
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|delay| executor.timer(delay))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, false, cx)
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        Host {
            outgoing,
            incoming,
            store,
            _pump: pump,
        }
    }

    fn send_settings(incoming: &async_channel::Sender<String>, settings: Settings) {
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
    }

    fn send_providers(incoming: &async_channel::Sender<String>, catalog: ProviderPluginCatalog) {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Providers,
                    event: ServerEvent::ProvidersReplaced(ProvidersStatus {
                        plugins: vec![catalog],
                        ..Default::default()
                    }),
                }))
                .unwrap(),
            )
            .unwrap();
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

    fn alpha() -> ProviderPluginEntry {
        ProviderPluginEntry {
            id: "alpha@mkt".into(),
            name: "alpha".into(),
            version: Some("1.0.0".into()),
            description: Some("Alpha plugin".into()),
            source: PluginSource {
                marketplace: Some("mkt".into()),
                kind: PluginSourceKind::LocalPath,
            },
            installations: vec![PluginInstallation {
                scope: PluginScope::Project,
                location: Some("/work/project/.claude/plugins/alpha".into()),
                version: Some("1.0.0".into()),
                scope_enabled: Some(true),
            }],
            enabled: Tri::Yes,
            declared: None,
            errors: Vec::new(),
            actions: vec![
                PluginAction::Install {
                    scope: PluginScope::User,
                },
                PluginAction::Disable {
                    scope: PluginScope::Project,
                },
            ],
            diagnostics: Vec::new(),
        }
    }

    /// An uninstalled plugin's row folds away where it was, and the row below
    /// it moves up as it goes rather than jumping into its place.
    #[gpui::test]
    fn an_uninstalled_plugin_folds_away_and_the_next_moves_up(cx: &mut TestAppContext) {
        let beta = ProviderPluginEntry {
            id: "beta@mkt".into(),
            name: "beta".into(),
            ..alpha()
        };
        let (host, cx) = open_plugins(
            claude_managed(),
            catalog(vec![alpha(), beta.clone()], Vec::new()),
            cx,
        );
        crate::test_frames::draw_settled(cx);
        let top = |row: &'static str, cx: &mut VisualTestContext| {
            cx.debug_bounds(row).expect("the row is drawn").top()
        };
        let first = top("plugin-row-claude-alpha@mkt", cx);
        let second = top("plugin-row-claude-beta@mkt", cx);

        host.replicate(catalog(vec![beta], Vec::new()), cx);
        assert!(
            cx.debug_bounds("plugin-row-claude-alpha@mkt").is_some(),
            "an uninstalled plugin is drawn while it leaves"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        crate::test_frames::draw(cx);
        let moving = top("plugin-row-claude-beta@mkt", cx);
        assert!(
            moving > first && moving < second,
            "beta is on its way up, at {moving:?}"
        );

        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("plugin-row-claude-alpha@mkt").is_none());
        assert_eq!(top("plugin-row-claude-beta@mkt", cx), first);
    }

    #[gpui::test]
    fn switching_language_relocalizes_the_search_placeholder(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        let mut uninstalled = alpha();
        uninstalled.installations.clear();
        let host = plugins_host(claude_managed(), catalog(vec![uninstalled], Vec::new()), cx);
        let window_state = cx.new(|_| WindowState::new(false));
        let store = host.store.clone();
        let (panel, cx) = cx.add_window_view(|window, cx| {
            PluginsSettingsPanel::new(store, window_state, ScrollHandle::new(), window, cx)
        });
        // The first frame shows the page, which lists the switched-on providers.
        draw(cx);
        cx.run_until_parked();
        host.store
            .update(cx, |store, cx| store.drain_host_events_for_test(cx));
        panel.update(cx, |panel, cx| {
            panel.view = View::Available;
            cx.notify();
        });
        crate::settings::assert_follows_language_switch(cx, "providers.plugins.search", |cx| {
            panel.read_with(cx, |panel, cx| {
                let search = panel.search.read(cx);
                search.presentation().placeholder().to_string()
            })
        });
    }

    /// The host decides which actions an entry offers in its context; the
    /// page draws exactly those and no control the host did not compute.
    #[gpui::test]
    fn only_host_computed_plugin_actions_are_rendered(cx: &mut TestAppContext) {
        let (host, cx) = open_plugins(claude_managed(), catalog(vec![alpha()], Vec::new()), cx);
        assert!(cx.debug_bounds("plugin-row-claude-alpha@mkt").is_some());

        let offered = [
            "plugin-install-claude-alpha@mkt-user",
            "plugin-disable-claude-alpha@mkt-project",
        ];
        for verb in ["install", "uninstall", "enable", "disable", "update"] {
            for scope in SCOPES {
                // Debug selectors are looked up by `'static` name.
                let id: &'static str =
                    format!("plugin-{verb}-claude-alpha@mkt-{}", scope_key(scope)).leak();
                assert_eq!(cx.debug_bounds(id).is_some(), offered.contains(&id), "{id}");
            }
        }
        drop(host);
    }

    /// A marketplace removal waits on the person: the dialog lists what it
    /// uninstalls, and Remove answers exactly that challenge, once.
    #[gpui::test]
    fn destructive_challenge_lists_uninstalls_and_accept_answers_it(cx: &mut TestAppContext) {
        let challenge = PluginChallenge {
            op_id: RuntimeOperationId(7),
            profile_id: "claude".into(),
            entry_id: "mkt".into(),
            kind: PluginChallengeKind::ConfirmDestructive {
                uninstalls: vec!["alpha@mkt".into(), "beta@mkt".into()],
            },
            native_text: None,
        };
        let pending = catalog(Vec::new(), vec![challenge]);
        let (host, cx) = open_plugins(claude_managed(), pending.clone(), cx);
        assert!(
            cx.debug_bounds("plugin-challenge-uninstall-alpha@mkt")
                .is_some()
        );
        assert!(
            cx.debug_bounds("plugin-challenge-uninstall-beta@mkt")
                .is_some()
        );
        host.commands(cx);

        let accept = cx.debug_bounds("plugin-challenge-accept").unwrap();
        cx.simulate_click(accept.center(), gpui::Modifiers::default());
        draw(cx);
        let answers: Vec<Command> = host
            .commands(cx)
            .into_iter()
            .filter(|command| matches!(command, Command::ResolvePluginChallenge { .. }))
            .collect();
        assert!(
            matches!(
                answers.as_slice(),
                [Command::ResolvePluginChallenge {
                    op_id: RuntimeOperationId(7),
                    accept: true
                }]
            ),
            "{answers:?}"
        );
        // The replica still shows the challenge until the host has acted on
        // the answer; the person is not asked again meanwhile.
        host.replicate(pending, cx);
        assert!(cx.debug_bounds("plugin-challenge-accept").is_none());
    }

    /// Off, a provider's section is its switch: a catalog still in the
    /// replica draws no controls and nothing is listed until the switch is
    /// turned on, which lists it.
    #[gpui::test]
    fn a_switched_off_provider_shows_only_its_switch_until_switched_on(cx: &mut TestAppContext) {
        let (host, cx) = open_plugins(Settings::default(), catalog(vec![alpha()], Vec::new()), cx);
        assert!(cx.debug_bounds("plugins-switch-claude").is_some());
        for id in [
            "plugin-row-claude-alpha@mkt",
            "plugin-install-claude-alpha@mkt-user",
            "plugin-disable-claude-alpha@mkt-project",
            "plugins-views-claude",
            "plugins-refresh-claude",
        ] {
            assert!(cx.debug_bounds(id).is_none(), "{id}");
        }
        let refreshed = |commands: &[Command]| -> Vec<String> {
            commands
                .iter()
                .filter_map(|command| match command {
                    Command::RefreshProviderPlugins { profile_id, .. } => Some(profile_id.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(refreshed(&host.commands(cx)), ["codex"]);

        let switch = cx.debug_bounds("plugins-switch-claude").unwrap();
        cx.simulate_click(switch.center(), gpui::Modifiers::default());
        draw(cx);
        let commands = host.commands(cx);
        assert!(
            commands.iter().any(|command| matches!(
                command,
                Command::PatchSettings {
                    patch: SettingsPatch::PluginManagementProvider {
                        provider: agent::ProviderKind::ClaudeCode,
                        enabled: true,
                    }
                }
            )),
            "{commands:?}"
        );
        host.replicate_settings(claude_managed(), cx);
        assert_eq!(refreshed(&host.commands(cx)), ["claude"]);
        assert!(cx.debug_bounds("plugins-views-claude").is_some());
    }
}
