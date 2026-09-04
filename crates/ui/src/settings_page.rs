//! Full-page settings route (V2-M6). Replaces the old settings dialog.
//!
//! When [`crate::Route::Settings`] is active, the whole window shows this
//! page: a left nav (same width as the sidebar) listing sections + a pinned
//! "← Back", and a content column of setting rows (bold title + muted
//! description on the left, a control on the right), matching reference shots
//! 40-settings.png / 41-settings-connections.png.

use std::collections::HashMap;
use std::rc::Rc;

use crate::overlay::{DialogButtons, OverlayExt as _};
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::switch::Switch;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Toggled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, v_flex};

use computer_use_mcp::permissions::{
    self, PermissionGrantAction, PermissionGrantFlow, PermissionKind, PermissionStatus,
    open_settings_pane, relaunch_app, request,
};

use crate::acp_panel::{AcpAgentCard, AcpPanel};
use crate::orchestrate_settings::OrchestrateSettingsPanel;
use crate::provider_card::ProviderCard;
use crate::provider_model_picker::ProviderModelPicker;
use crate::settings::{ImageMode, LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE, ThemeMode};
use crate::shell::Quit;
use crate::store::WorkspaceStore;
use crate::theme::{self, ActiveTheme as _, ThemeMode as UiThemeMode};
use crate::time::{humanize_ago, now_secs};
use crate::window_caption;
use crate::window_drag_area;
use crate::window_state::WindowState;
use tcode_core::settings::{
    DEFAULT_AUTO_ARCHIVE_KEEP_COUNT, DEFAULT_AUTO_ARCHIVE_MAX_IDLE_DAYS, FallbackReviewSettings,
    TitleGenerationSettings,
};

/// Left inset so branding clears the native macOS 26 traffic lights near x=72.
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_INSET: f32 = 80.;
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHT_INSET: f32 = 8.;

/// Width of the settings left-nav column (matches the sidebar width).
const NAV_WIDTH: f32 = 255.;
/// Max width of the settings content column — matches the chat timeline column
/// (`chat::CONTENT_MAX_WIDTH`) so the reading measure is identical across routes.
const CONTENT_MAX_WIDTH: f32 = 768.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    Providers,
    Usage,
    Browser,
    ComputerUse,
    Orchestrate,
    Archived,
}

#[derive(Clone)]
struct SelectRowOption<T> {
    value: T,
    id: SharedString,
    label: SharedString,
    description: Option<SharedString>,
    selected: bool,
}

/// Apply a settings theme mode to the live window (shared with the palette's
/// "Toggle theme" action).
pub(crate) fn apply_theme(mode: ThemeMode, window: &mut Window, cx: &mut App) {
    match mode {
        ThemeMode::Light => theme::change_mode(UiThemeMode::Light, Some(window), cx),
        ThemeMode::Dark => theme::change_mode(UiThemeMode::Dark, Some(window), cx),
        ThemeMode::System => theme::sync_system_appearance(Some(window), cx),
    }
}

pub struct SettingsPage {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// One card per native profile, keyed by profile id (built-in + user).
    provider_cards: Vec<(String, Entity<ProviderCard>)>,
    /// Long-lived state for the modal ACP marketplace and custom form.
    acp_panel: Entity<AcpPanel>,
    /// Editable main-model identities and child-model routing matrix.
    orchestrate_panel: Entity<OrchestrateSettingsPanel>,
    /// Shared provider/model picker configured for background thread titles.
    title_model_picker: Entity<ProviderModelPicker>,
    /// Shared provider/model picker configured for fallback reviews.
    fallback_review_model_picker: Entity<ProviderModelPicker>,
    /// Stable entities keep expanded state and lazily-created inputs across rerenders.
    acp_cards: Vec<(String, Entity<AcpAgentCard>)>,
    section: Section,
    /// Latches the one usage refresh fired when Usage becomes the active
    /// section; cleared as soon as the page shows anything else.
    usage_refresh_sent: bool,
    /// Editable "Home URL" for the Browser page; committed on change.
    home_url_input: Entity<InputState>,
    auto_archive_idle_input: Entity<InputState>,
    auto_archive_keep_input: Entity<InputState>,
    /// Last-known TCC permission snapshot, refreshed when Computer Use becomes
    /// visible and on every explicit Recheck / Grant.
    perm_status: PermissionStatus,
    /// Whether a Screen Recording grant looks pending-restart (a fresh grant
    /// only takes effect after tcode relaunches). Drives the restart banner.
    sr_restart_hint: bool,
    /// A temporary continuity marker exists for an in-flight Screen Recording
    /// request. It is cleared when the app becomes active without a grant.
    screen_recording_marker_pending: bool,
    /// Separates the initial native TCC request from the explicit fallback that
    /// opens System Settings when macOS will no longer show its prompt.
    permission_grant_flow: PermissionGrantFlow,
    _app_activation_observer: crate::app_activation::AppActivationObserver,
    /// One focus handle per toggle row, keyed by row id. The row owns keyboard
    /// activation, so its capture-phase Space handler must be able to tell
    /// "the row is focused" from "the inline reset button inside it is".
    toggle_focus: HashMap<&'static str, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    fn take_requested_section(
        window_state: &Entity<WindowState>,
        cx: &mut Context<Self>,
    ) -> Option<Section> {
        window_state
            .update(cx, |state, _| state.pending_settings_section.take())
            .map(|section| match section.as_str() {
                "providers" => Section::Providers,
                "usage" => Section::Usage,
                "browser" => Section::Browser,
                "computer_use" => Section::ComputerUse,
                "orchestrate" => Section::Orchestrate,
                "archived" => Section::Archived,
                _ => Section::General,
            })
    }

    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title_generation = store.read(cx).settings().title_generation;
        let title_model_picker = cx.new(|cx| {
            ProviderModelPicker::selection(
                store.clone(),
                "title-model-popover",
                "title-model-dropdown",
                title_generation.provider,
                title_generation.model,
                title_generation.profile_id,
                cx,
            )
        });
        let fallback_review = store.read(cx).settings().fallback_review;
        let fallback_review_model_picker = cx.new(|cx| {
            ProviderModelPicker::selection(
                store.clone(),
                "fallback-review-model-popover",
                "fallback-review-model-dropdown",
                fallback_review.provider,
                fallback_review.model,
                fallback_review.profile_id,
                cx,
            )
        });
        let subscriptions = vec![
            cx.observe(&store, |this, _, cx| {
                let selection = this.store.read(cx).settings().title_generation;
                this.title_model_picker.update(cx, |picker, cx| {
                    picker.set_selected(
                        selection.provider,
                        selection.model,
                        selection.profile_id,
                        cx,
                    );
                });
                cx.notify();
            }),
            cx.observe(&store, |this, _, cx| {
                let selection = this.store.read(cx).settings().fallback_review;
                this.fallback_review_model_picker.update(cx, |picker, cx| {
                    picker.set_selected(
                        selection.provider,
                        selection.model,
                        selection.profile_id,
                        cx,
                    );
                });
                cx.notify();
            }),
            cx.observe(&window_state, |this, _, cx| {
                let window_state = this.window_state.clone();
                if let Some(section) = Self::take_requested_section(&window_state, cx) {
                    this.section = section;
                }
                cx.notify();
            }),
            cx.subscribe(&title_model_picker, |this, _, event, cx| {
                let selected = event.0.clone();
                this.dispatch_settings(
                    move |store| {
                        store.set_title_generation_provider(selected.provider);
                        store.set_title_generation_model(selected.id);
                        store.set_title_generation_profile_id(selected.profile_id);
                    },
                    cx,
                );
            }),
            cx.subscribe(&fallback_review_model_picker, |this, _, event, cx| {
                let selected = event.0.clone();
                this.dispatch_settings(
                    move |store| {
                        store.set_fallback_review_provider(selected.provider);
                        store.set_fallback_review_model(selected.id);
                        store.set_fallback_review_profile_id(selected.profile_id);
                    },
                    cx,
                );
            }),
        ];

        // Consume relaunch and in-app navigation requests through the same
        // channel used by in-app Settings links.
        let section = Self::take_requested_section(&window_state, cx).unwrap_or(Section::General);
        let acp_panel = cx.new(|cx| AcpPanel::new(store.clone(), window, cx));
        let orchestrate_panel =
            cx.new(|cx| OrchestrateSettingsPanel::new(store.clone(), window, cx));
        let settings = store.read(cx).settings();
        let home_url_value = settings.browser.home_url.clone().unwrap_or_default();
        let home_url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::tr!("browser.home_url.placeholder"))
                .default_value(home_url_value)
        });
        let auto_archive_idle_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.auto_archive_max_idle_days.max(1).to_string())
        });
        let auto_archive_keep_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.auto_archive_keep_count.max(1).to_string())
        });
        // Refresh the TCC snapshot once as the page mounts. When the page is
        // opened by a post-grant relaunch this is the "automatic recheck" that
        // surfaces the new status immediately.
        let perm_status = permissions::check();
        let (app_activation_observer, app_activation_events) = crate::app_activation::observe();
        let mut page = Self {
            store,
            window_state,
            provider_cards: Vec::new(),
            acp_panel,
            orchestrate_panel,
            title_model_picker,
            fallback_review_model_picker,
            acp_cards: Vec::new(),
            section,
            usage_refresh_sent: false,
            home_url_input: home_url_input.clone(),
            auto_archive_idle_input: auto_archive_idle_input.clone(),
            auto_archive_keep_input: auto_archive_keep_input.clone(),
            perm_status,
            sr_restart_hint: false,
            screen_recording_marker_pending: false,
            permission_grant_flow: PermissionGrantFlow::default(),
            _app_activation_observer: app_activation_observer,
            toggle_focus: HashMap::new(),
            _subscriptions: subscriptions,
        };
        page._subscriptions
            .push(cx.subscribe(&home_url_input, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_home_url(cx);
                }
            }));
        page._subscriptions.push(
            cx.subscribe(&auto_archive_idle_input, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_auto_archive_idle_days(cx);
                }
            }),
        );
        page._subscriptions.push(
            cx.subscribe(&auto_archive_keep_input, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_auto_archive_keep_count(cx);
                }
            }),
        );
        page.build_provider_cards(cx);
        page.sync_acp_cards(window, cx);
        cx.spawn(async move |this, cx| {
            while app_activation_events.recv().await.is_ok() {
                if this
                    .update(cx, |this, cx| {
                        if this.section == Section::ComputerUse {
                            this.recheck_permissions(true, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        page
    }

    /// Persist the Browser "Home URL" field (empty → `None`).
    fn commit_home_url(&self, cx: &mut Context<Self>) {
        let value = self.home_url_input.read(cx).value().trim().to_string();
        let home_url = (!value.is_empty()).then_some(value);
        self.dispatch_settings(move |store| store.set_browser_home_url(home_url), cx);
    }

    fn commit_auto_archive_idle_days(&self, cx: &mut Context<Self>) {
        let Some(days) = self
            .auto_archive_idle_input
            .read(cx)
            .value()
            .trim()
            .parse::<u32>()
            .ok()
        else {
            return;
        };
        self.dispatch_settings(
            move |store| store.set_auto_archive_max_idle_days(days.max(1)),
            cx,
        );
    }

    fn commit_auto_archive_keep_count(&self, cx: &mut Context<Self>) {
        let Some(keep) = self
            .auto_archive_keep_input
            .read(cx)
            .value()
            .trim()
            .parse::<usize>()
            .ok()
        else {
            return;
        };
        self.dispatch_settings(
            move |store| store.set_auto_archive_keep_count(keep.max(1)),
            cx,
        );
    }

    /// (Re)build the provider cards from current settings.
    fn build_provider_cards(&mut self, cx: &mut Context<Self>) {
        let profiles = self.store.read(cx).all_provider_profiles();
        self.provider_cards = profiles
            .into_iter()
            .map(|profile| {
                let store = self.store.clone();
                let kind = profile.kind;
                let id = profile.id.clone();
                let card = cx.new(|cx| ProviderCard::new(store, kind, id, cx));
                (profile.id, card)
            })
            .collect();
    }

    /// Reconcile card entities by installed id, preserving editors for agents
    /// that were not installed or removed since the previous render.
    fn sync_acp_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let installed: Vec<_> = self.store.read(cx).settings_installed_acp_agents();
        let mut old = std::mem::take(&mut self.acp_cards);
        self.acp_cards = installed
            .into_iter()
            .map(|agent| {
                let card = old
                    .iter()
                    .position(|(id, _)| id == &agent.id)
                    .map(|index| old.swap_remove(index).1)
                    .unwrap_or_else(|| {
                        let store = self.store.clone();
                        cx.new(|cx| AcpAgentCard::new(store, &agent, window, cx))
                    });
                (agent.id, card)
            })
            .collect();
    }

    fn open_acp_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.acp_panel
            .update(cx, |panel, cx| panel.prepare_to_open(cx));
        let panel = self.acp_panel.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let panel = panel.clone();
            dialog
                .w(px(620.))
                // Opaque T3 panel: the library default paints the translucent
                // glass canvas, which lets the page bleed through.
                .bg(cx.theme().popover)
                .shadow_xl()
                .title(crate::tr!("providers.acp.add_agent").into_owned())
                .content(move |content, _, _| content.h(px(456.)).child(panel.clone()))
        });
    }

    fn dispatch_settings(&self, intent: impl FnOnce(&mut WorkspaceStore), cx: &mut Context<Self>) {
        self.store.update(cx, |store, _cx| intent(store));
    }

    // -- left nav -----------------------------------------------------------

    fn render_nav(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let nav_item = |this: &Self,
                        id: &'static str,
                        icon: IconName,
                        label: SharedString,
                        section: Section,
                        cx: &mut Context<Self>|
         -> AnyElement {
            let active = this.section == section;
            let fg = if active {
                cx.theme().sidebar_foreground
            } else {
                cx.theme().muted_foreground
            };
            crate::material::accessible_clickable(
                gpui_base::h_flex(),
                id,
                Role::Tab,
                label.clone(),
                cx,
            )
            .aria_selected(active)
            // Keep the hitbox, stable element id, and hover style on the
            // same element. Splitting them across an outer clickable and
            // an anonymous inner row leaves GPUI tracking two overlapping
            // interaction regions, which makes hover paint stale or skip
            // as the pointer crosses adjacent tabs.
            .h(px(30.))
            .items_center()
            .gap_2()
            .px_2()
            .rounded(px(6.))
            .cursor_pointer()
            .when(active, |s| s.bg(cx.theme().list_active))
            .when(!active, |s| s.hover(|s| s.bg(cx.theme().sidebar_accent)))
            .child(Icon::new(icon).size_4().text_color(fg))
            .child(
                div()
                    .text_size(px(13.))
                    .when(active, |d| d.font_medium())
                    .text_color(fg)
                    .child(label.clone()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.section = section;
                // Refresh the TCC snapshot each time Computer Use becomes
                // visible (cheap native calls, event-driven).
                if section == Section::ComputerUse {
                    this.perm_status = permissions::check();
                }
                cx.notify();
            }))
            .into_any_element()
        };

        v_flex()
            .flex_none()
            .w(px(NAV_WIDTH))
            .h_full()
            .bg(cx.theme().sidebar)
            .child(
                window_drag_area(
                    "settings-nav-drag",
                    gpui_base::h_flex()
                        .h(px(52.))
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .pl(px(TRAFFIC_LIGHT_INSET))
                        .pr_2(),
                    window,
                    cx,
                )
                // Same brand chrome as the main sidebar's app row (DEV pill
                // included) so the settings nav reads as the same family.
                .child(crate::material::brand_wordmark(cx)),
            )
            .child(
                v_flex()
                    .id("settings-nav-tabs")
                    .role(Role::TabList)
                    .aria_label(crate::tr!("settings.title"))
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .gap(px(2.))
                    .child(nav_item(
                        self,
                        "settings-nav-general",
                        IconName::Settings,
                        crate::tr!("settings.general").into_owned().into(),
                        Section::General,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-providers",
                        IconName::Bot,
                        crate::tr!("settings.providers").into_owned().into(),
                        Section::Providers,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-usage",
                        IconName::ChartPie,
                        crate::tr!("settings.usage").into_owned().into(),
                        Section::Usage,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-browser",
                        IconName::Globe,
                        crate::tr!("settings.browser").into_owned().into(),
                        Section::Browser,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-computer-use",
                        IconName::LayoutDashboard,
                        crate::tr!("settings.computer_use").into_owned().into(),
                        Section::ComputerUse,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-orchestrate",
                        IconName::Map,
                        crate::tr!("settings.orchestrate").into_owned().into(),
                        Section::Orchestrate,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-archived",
                        IconName::Inbox,
                        crate::tr!("settings.archived").into_owned().into(),
                        Section::Archived,
                        cx,
                    )),
            )
            .child(
                div().flex_none().child(
                    crate::material::accessible_clickable(
                        gpui_base::h_flex(),
                        "settings-back",
                        Role::Button,
                        crate::tr!("settings.back"),
                        cx,
                    )
                    // Mirror the main sidebar footer (the "Settings" entry that
                    // enters this route): same 40px height, muted leading icon.
                    .h(px(40.))
                    .items_center()
                    .gap_2()
                    .px_3()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().sidebar_accent))
                    .text_size(px(13.))
                    .text_color(cx.theme().sidebar_foreground)
                    .child(
                        Icon::new(IconName::ArrowLeft)
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(crate::tr!("settings.back"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.window_state
                            .update(cx, |state, cx| state.close_settings(cx));
                    })),
                ),
            )
            .into_any_element()
    }

    // -- content ------------------------------------------------------------

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // Windows: the settings route replaces the workspace, so this header is
        // the window's top-right corner and hosts the caption buttons. The
        // centered column must keep its position (it aligns with the content
        // below), so the cluster is placed out of flow at the strip's right edge
        // and the column reserves matching trailing room for it — its actions
        // therefore end left of the buttons rather than under them.
        let (right_panel_open, right_tab) = self.store.read(cx).window_caption_state();
        let hosts_caption = window_caption::hosts_caption_for_state(
            window_caption::CaptionSurface::Settings,
            self.window_state.read(cx).route,
            right_panel_open,
            right_tab,
        );
        // The 52px strip spans the paper full-width (drag area), but its title
        // and actions ride the same centered 768px column as the content below,
        // the way the chat header aligns with its timeline column.
        window_drag_area(
            "settings-header-drag",
            gpui_base::h_flex()
                .flex_none()
                .h(px(52.))
                .w_full()
                .px_6()
                .justify_center()
                .items_center()
                .when(hosts_caption, |strip| strip.relative()),
            window,
            cx,
        )
        .child(
            gpui_base::h_flex()
                .w(px(CONTENT_MAX_WIDTH))
                .max_w_full()
                .when(hosts_caption, |column| {
                    column.pr(px(window_caption::CAPTION_CLUSTER_WIDTH))
                })
                .items_center()
                .gap_3()
                // The strip carries no controls at all (restoring defaults now
                // lives at the foot of the General page), so the whole title
                // doubles as the window's native drag handle.
                .child(window_caption::drag_region(
                    div()
                        .flex_1()
                        .text_size(px(15.))
                        .font_medium()
                        .child(crate::tr!("settings.title")),
                )),
        )
        // Painted last so the cluster stays on top of the strip.
        .children(hosts_caption.then(|| {
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .child(window_caption::caption_controls(window, cx))
        }))
        .into_any_element()
    }

    fn confirm_restore(&self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let page = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let alert = alert.bg(cx.theme().popover);
            let store = store.clone();
            let page = page.clone();
            alert
                .title(crate::tr!("settings.restore_title"))
                .description(crate::tr!("settings.restore_description"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("settings.restore"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    store.update(cx, |store, _cx| {
                        store.reset_settings();
                    });
                    // Provider profiles and installed agents survive the reset,
                    // so their cards need no rebuild — only the panels and
                    // inputs holding a copy of a reset preference do.
                    page.update(cx, |page, cx| {
                        let store = page.store.clone();
                        page.orchestrate_panel =
                            cx.new(|cx| OrchestrateSettingsPanel::new(store, window, cx));
                        // The Home URL input now holds a stale override.
                        let home_url = page
                            .store
                            .read(cx)
                            .settings()
                            .browser
                            .home_url
                            .clone()
                            .unwrap_or_default();
                        page.home_url_input
                            .update(cx, |input, cx| input.set_value(home_url, window, cx));
                        page.auto_archive_idle_input.update(cx, |input, cx| {
                            input.set_value(
                                DEFAULT_AUTO_ARCHIVE_MAX_IDLE_DAYS.to_string(),
                                window,
                                cx,
                            )
                        });
                        page.auto_archive_keep_input.update(cx, |input, cx| {
                            input.set_value(DEFAULT_AUTO_ARCHIVE_KEEP_COUNT.to_string(), window, cx)
                        });
                    });
                    apply_theme(ThemeMode::System, window, cx);
                    true
                })
        });
    }

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // Usage is fetched on demand: kick one refresh off on the transition
        // into the section (any entry path — nav click, route key, restore),
        // not on every frame it renders.
        if self.section == Section::Usage {
            if !self.usage_refresh_sent {
                self.usage_refresh_sent = true;
                self.store
                    .update(cx, |store, _cx| store.refresh_provider_usage());
            }
        } else {
            self.usage_refresh_sent = false;
        }
        let column = match self.section {
            Section::General => self.render_general(cx),
            Section::Providers => self.render_providers(window, cx),
            Section::Usage => self.render_usage(cx),
            Section::Browser => self.render_browser(cx),
            Section::ComputerUse => self.render_computer_use(cx),
            Section::Orchestrate => v_flex().child(self.orchestrate_panel.clone()),
            Section::Archived => self.render_archived(cx),
        };
        div()
            .id("settings-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                gpui_base::h_flex()
                    .w_full()
                    .justify_center()
                    .px_6()
                    .py_6()
                    // Keep this width definite before capping it. Reversing
                    // these constraints makes nested multiline inputs resolve
                    // their percentage width to zero when the cap applies.
                    .child(column.w(px(CONTENT_MAX_WIDTH)).max_w_full()),
            )
            .into_any_element()
    }

    fn render_general(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        // One mega-group on empty paper reads generic. Split the rows into three
        // semantic groups (System-Settings rhythm): 20-24px between groups, each
        // under an 11px caption.
        let appearance = vec![
            self.language_row(settings.language.as_deref(), cx),
            self.theme_row(settings.theme_mode, cx),
        ];
        let delete_confirm_reset = self.reset_action(
            "reset-delete-confirm",
            settings.skip_delete_confirmation,
            cx,
            |this, _, cx| {
                this.dispatch_settings(|store| store.set_skip_delete_confirmation(false), cx)
            },
        );
        let auto_open_task_panel_reset = self.reset_action(
            "reset-auto-open-task-panel",
            settings.auto_open_task_panel,
            cx,
            |this, _, cx| this.dispatch_settings(|store| store.set_auto_open_task_panel(false), cx),
        );
        let live_command_panel_reset = self.reset_action(
            "reset-live-command-panel",
            settings.live_command_panel_disabled,
            cx,
            |this, _, cx| {
                this.dispatch_settings(|store| store.set_live_command_panel_disabled(false), cx)
            },
        );
        let abort_on_fallback_reset = self.reset_action(
            "reset-abort-on-model-fallback",
            !settings.abort_on_model_fallback,
            cx,
            |this, _, cx| {
                this.dispatch_settings(|store| store.set_abort_on_model_fallback(true), cx)
            },
        );
        let mut conversation = vec![
            self.title_generation_row(cx),
            self.toggle_row(
                "delete-confirm",
                crate::tr!("settings.delete_confirmation.title"),
                crate::tr!("settings.delete_confirmation.description"),
                !settings.skip_delete_confirmation,
                delete_confirm_reset,
                cx,
                |store, checked| store.set_skip_delete_confirmation(!checked),
            ),
            self.toggle_row(
                "auto-open-task-panel",
                crate::tr!("settings.auto_open_task_panel.title"),
                crate::tr!("settings.auto_open_task_panel.description"),
                settings.auto_open_task_panel,
                auto_open_task_panel_reset,
                cx,
                WorkspaceStore::set_auto_open_task_panel,
            ),
            self.toggle_row(
                "live-command-panel",
                crate::tr!("settings.live_command_panel.title"),
                crate::tr!("settings.live_command_panel.description"),
                !settings.live_command_panel_disabled,
                live_command_panel_reset,
                cx,
                |store, checked| store.set_live_command_panel_disabled(!checked),
            ),
            self.toggle_row(
                "abort-on-model-fallback",
                crate::tr!("settings.abort_on_model_fallback.title"),
                crate::tr!("settings.abort_on_model_fallback.description"),
                settings.abort_on_model_fallback,
                abort_on_fallback_reset,
                cx,
                WorkspaceStore::set_abort_on_model_fallback,
            ),
        ];
        if settings.abort_on_model_fallback {
            let advisor_reset = self.reset_action(
                "reset-fallback-review-advisor",
                settings.fallback_review_advisor,
                cx,
                |this, _, cx| {
                    this.dispatch_settings(|store| store.set_fallback_review_advisor(false), cx)
                },
            );
            conversation.push(self.toggle_row(
                "fallback-review-advisor",
                crate::tr!("settings.fallback_review_advisor.title"),
                crate::tr!("settings.fallback_review_advisor.description"),
                settings.fallback_review_advisor,
                advisor_reset,
                cx,
                WorkspaceStore::set_fallback_review_advisor,
            ));
            conversation.push(self.fallback_review_model_row(cx));
        }
        let word_wrap_reset = self.reset_action(
            "reset-word-wrap",
            settings.word_wrap_diffs,
            cx,
            |this, _, cx| this.dispatch_settings(|store| store.set_word_wrap_diffs(false), cx),
        );
        let provider_updates_reset = self.reset_action(
            "reset-provider-update-checks",
            settings.provider_update_checks_disabled,
            cx,
            |this, _, cx| {
                this.dispatch_settings(|store| store.set_provider_update_checks_disabled(false), cx)
            },
        );
        let frame_throttle_reset = self.reset_action(
            "reset-inactive-frame-throttle",
            settings.inactive_frame_throttle_disabled,
            cx,
            |this, _, cx| {
                this.dispatch_settings(
                    |store| store.set_inactive_frame_throttle_disabled(false),
                    cx,
                )
            },
        );
        let workspace = vec![
            self.toggle_row(
                "word-wrap",
                crate::tr!("settings.word_wrap.title"),
                crate::tr!("settings.word_wrap.description"),
                settings.word_wrap_diffs,
                word_wrap_reset,
                cx,
                WorkspaceStore::set_word_wrap_diffs,
            ),
            self.toggle_row(
                "provider-update-checks",
                crate::tr!("settings.provider_updates.title"),
                crate::tr!("settings.provider_updates.description"),
                // Stored inverted: checked = enabled.
                !settings.provider_update_checks_disabled,
                provider_updates_reset,
                cx,
                |store, checked| store.set_provider_update_checks_disabled(!checked),
            ),
            self.toggle_row(
                "inactive-frame-throttle",
                crate::tr!("settings.inactive_frame_throttle.title"),
                crate::tr!("settings.inactive_frame_throttle.description"),
                // Stored inverted: checked = enabled.
                !settings.inactive_frame_throttle_disabled,
                frame_throttle_reset,
                cx,
                |store, checked| store.set_inactive_frame_throttle_disabled(!checked),
            ),
        ];
        v_flex()
            .gap(px(24.))
            .child(
                v_flex()
                    .child(self.section_label(crate::tr!("settings.appearance_section"), cx))
                    .child(self.grouped_plain(appearance, cx)),
            )
            .child(
                v_flex()
                    .child(self.section_label(crate::tr!("settings.conversation_section"), cx))
                    .child(self.grouped_plain(conversation, cx)),
            )
            .child(
                v_flex()
                    .child(self.section_label(crate::tr!("settings.workspace_section"), cx))
                    .child(self.grouped_plain(workspace, cx)),
            )
            .child(
                v_flex()
                    .child(self.section_label(crate::tr!("settings.reset_section"), cx))
                    .child(self.grouped_plain(vec![self.restore_all_row(cx)], cx)),
            )
    }

    /// The whole-app escape hatch, parked at the foot of General: the per-item
    /// buttons cover single settings, this one covers the rest and keeps its
    /// confirm dialog.
    fn restore_all_row(&self, cx: &mut Context<Self>) -> AnyElement {
        self.row_frame(cx)
            .child(self.row_labels(
                crate::tr!("settings.reset_all"),
                crate::tr!("settings.restore_description"),
                None,
                cx,
            ))
            .child(
                Button::new("restore-defaults")
                    .danger()
                    .outline()
                    .small()
                    .label(crate::tr!("settings.restore"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.confirm_restore(window, cx);
                    })),
            )
            .into_any_element()
    }

    fn title_generation_row(&self, cx: &mut Context<Self>) -> AnyElement {
        // Provider, model and profile are one setting to the user, so they
        // reset together; the picker follows through the store observer.
        let overridden =
            self.store.read(cx).settings().title_generation != TitleGenerationSettings::default();
        let reset = self.reset_action("reset-title-generation", overridden, cx, |this, _, cx| {
            let default = TitleGenerationSettings::default();
            this.dispatch_settings(
                move |store| {
                    store.set_title_generation_provider(default.provider);
                    store.set_title_generation_model(default.model);
                    store.set_title_generation_profile_id(default.profile_id);
                },
                cx,
            );
        });
        self.row_frame(cx)
            .child(self.row_labels(
                crate::tr!("settings.title_generation.title"),
                crate::tr!("settings.title_generation.description"),
                reset,
                cx,
            ))
            .child(self.title_model_picker.clone())
            .into_any_element()
    }

    fn fallback_review_model_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let overridden =
            self.store.read(cx).settings().fallback_review != FallbackReviewSettings::default();
        let reset = self.reset_action(
            "reset-fallback-review-model",
            overridden,
            cx,
            |this, _, cx| {
                let default = FallbackReviewSettings::default();
                this.dispatch_settings(
                    move |store| {
                        store.set_fallback_review_provider(default.provider);
                        store.set_fallback_review_model(default.model);
                        store.set_fallback_review_profile_id(default.profile_id);
                    },
                    cx,
                );
            },
        );
        self.row_frame(cx)
            .child(self.row_labels(
                crate::tr!("settings.fallback_review_model.title"),
                crate::tr!("settings.fallback_review_model.description"),
                reset,
                cx,
            ))
            .child(self.fallback_review_model_picker.clone())
            .into_any_element()
    }

    fn render_tcode_update_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.store.read(cx).tcode_update_status();
        let version = status.latest.unwrap_or_default();
        let release_url = status.release_url.unwrap_or_default();

        crate::material::overlay_popover("tcode-update-popover")
            // Prose card, not a menu: keep the roomier panel padding.
            .p_3()
            .trigger(
                Button::new("tcode-update-available")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/download.svg"))
                    .label(crate::tr!(
                        "providers.tcode_update_available",
                        version = version
                    ))
                    .tooltip(crate::tr!("providers.tcode_update_aria")),
            )
            .content(move |_, _, cx| {
                let muted = cx.theme().muted_foreground;
                let release_url = release_url.clone();
                v_flex()
                    .w(px(320.))
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .child(crate::tr!("providers.tcode_update_title")),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(muted)
                            .child(crate::tr!("providers.tcode_update_message")),
                    )
                    .child(
                        Button::new("view-tcode-release")
                            .primary()
                            .small()
                            .label(crate::tr!("providers.view_release"))
                            .on_click(move |_, _, cx| cx.open_url(&release_url)),
                    )
            })
            .into_any_element()
    }

    /// Settings → Providers: native providers and installed ACP agents share one
    /// bordered list. The marketplace lives behind the Add agent dialog.
    fn render_providers(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        self.sync_acp_cards(window, cx);
        // Reconcile the profile cards with the current profile set: creating or
        // deleting a profile changes it, and the card list must follow (else a
        // deleted profile's card lingers and its Delete looks like a no-op).
        let current_ids: Vec<String> = self
            .store
            .read(cx)
            .all_provider_profiles()
            .into_iter()
            .map(|profile| profile.id)
            .collect();
        let card_ids: Vec<String> = self
            .provider_cards
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        if current_ids != card_ids {
            self.build_provider_cards(cx);
        }
        let checked_at = self.store.read(cx).providers_checked_at();
        let checking = self.store.read(cx).providers_checking();
        let muted = cx.theme().muted_foreground;

        let mut header = gpui_base::h_flex().w_full().items_center().gap_2().child(
            div()
                .flex_1()
                .pl_3()
                .text_size(px(11.))
                .font_medium()
                .text_color(muted)
                .child(crate::tr!("settings.providers_section")),
        );
        if let Some(checked_at) = checked_at {
            let ago = humanize_ago(now_secs().saturating_sub(checked_at));
            header = header.child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(crate::tr!("providers.checked", when = ago).into_owned()),
            );
        }
        if self.store.read(cx).tcode_update_status().update_available {
            header = header.child(self.render_tcode_update_popover(cx));
        }
        header = header.child(
            Button::new("add-acp-agent")
                .outline()
                .xsmall()
                .icon(IconName::Plus)
                .label(crate::tr!("providers.acp.add_agent").into_owned())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_acp_dialog(window, cx);
                })),
        );
        header = header.child(
            Button::new("refresh-providers")
                .ghost()
                .xsmall()
                .loading(checking)
                .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                .tooltip(crate::tr!("providers.refresh"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store.update(cx, |store, _cx| {
                        store.refresh_provider_status();
                        store.check_provider_versions();
                    });
                })),
        );

        // Native providers form one grouped list; each card is a compact row
        // whose gear / body click opens the per-profile settings dialog.
        let provider_rows: Vec<AnyElement> = self
            .provider_cards
            .iter()
            .map(|(_, card)| card.clone().into_any_element())
            .collect();

        let mut section = v_flex()
            .w_full()
            .gap_3()
            .child(header)
            .child(crate::material::grouped(provider_rows, cx));
        // ACP agent cards keep their own component styling (defined outside this
        // file); they sit beneath the native providers in the same section.
        for (_, card) in &self.acp_cards {
            section = section.child(card.clone());
        }
        section
    }

    /// Usage: one card per enabled Codex / Claude Code profile listing the
    /// account rate-limit windows the provider reported — never more, never a
    /// synthesized one (a Codex Pro account genuinely has no 5h window).
    fn render_usage(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let store = self.store.read(cx);
        let profiles: Vec<_> = store
            .enabled_profiles()
            .into_iter()
            .filter(|profile| {
                matches!(
                    profile.kind,
                    agent::ProviderKind::Codex | agent::ProviderKind::ClaudeCode
                )
            })
            .collect();
        let rows: Vec<(String, String, agent::ProviderKind, _, bool)> = profiles
            .iter()
            .map(|profile| {
                (
                    profile.id.clone(),
                    store.provider_profile_display_name(&profile.id),
                    profile.kind,
                    store.provider_usage(&profile.id),
                    store.usage_checking(&profile.id),
                )
            })
            .collect();
        let updated_at = rows
            .iter()
            .filter_map(|(_, _, _, usage, _)| usage.as_ref().map(|usage| usage.fetched_at))
            .max();
        let checking = rows.iter().any(|(_, _, _, _, checking)| *checking);
        let muted = cx.theme().muted_foreground;

        let mut header = gpui_base::h_flex().w_full().items_center().gap_2().child(
            div()
                .flex_1()
                .pl_3()
                .text_size(px(11.))
                .font_medium()
                .text_color(muted)
                .child(crate::tr!("usage.section")),
        );
        if let Some(updated_at) = updated_at {
            let ago = humanize_ago(now_secs().saturating_sub(updated_at));
            header = header.child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(crate::tr!("usage.updated", when = ago).into_owned()),
            );
        }
        header = header.child(
            Button::new("refresh-usage")
                .ghost()
                .xsmall()
                .loading(checking)
                .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                .tooltip(crate::tr!("usage.refresh"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store
                        .update(cx, |store, _cx| store.refresh_provider_usage());
                })),
        );

        let mut section = v_flex().w_full().gap_3().child(header);
        if rows.is_empty() {
            section = section.child(crate::material::grouped(
                vec![self.usage_note_row(crate::tr!("usage.no_profiles").into_owned(), None, cx)],
                cx,
            ));
        }
        for (profile_id, name, kind, usage, checking) in rows {
            let mut card_rows = vec![self.usage_card_header(
                &name,
                kind,
                usage.as_ref().and_then(|u| u.plan.clone()),
                cx,
            )];
            match &usage {
                Some(usage) if usage.error.is_some() => card_rows.push(self.usage_note_row(
                    crate::tr!("usage.unavailable").into_owned(),
                    usage.error.clone(),
                    cx,
                )),
                Some(usage) if !usage.windows.is_empty() => {
                    let now = now_secs();
                    card_rows.extend(
                        usage
                            .windows
                            .iter()
                            .map(|window| self.usage_window_row(window, now, cx)),
                    );
                }
                _ if checking => card_rows.push(self.usage_note_row(
                    crate::tr!("usage.checking").into_owned(),
                    None,
                    cx,
                )),
                _ => card_rows.push(self.usage_note_row(
                    crate::tr!("usage.no_data").into_owned(),
                    None,
                    cx,
                )),
            }
            section = section.child(
                div()
                    .id(SharedString::from(format!("usage-card-{profile_id}")))
                    .child(crate::material::grouped(card_rows, cx)),
            );
        }
        section
    }

    /// A usage card's title row: provider glyph + profile name + plan chip.
    fn usage_card_header(
        &self,
        name: &str,
        kind: agent::ProviderKind,
        plan: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        self.row_frame(cx)
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_none()
                    .size(px(16.))
                    .child(crate::provider_card::provider_glyph(kind).small()),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .font_medium()
                    .child(name.to_owned()),
            )
            .when_some(plan, |row, plan| {
                row.child(crate::material::semantic_chip(
                    crate::usage::plan_label(&plan),
                    cx.theme().muted,
                    muted,
                ))
            })
            .into_any_element()
    }

    /// One rate-limit window: label + "resets in …" on the left, percent on
    /// the right, a 6px bar underneath.
    fn usage_window_row(
        &self,
        window: &tcode_core::usage::UsageWindow,
        now: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let fill = crate::usage::bar_color(window.used_percent, cx);
        self.row_frame(cx)
            .flex_col()
            .items_start()
            .gap_2()
            .child(
                gpui_base::h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_medium()
                                    .child(crate::usage::window_label(window)),
                            )
                            .when_some(
                                crate::usage::resets_label(window.resets_at, now),
                                |col, label| {
                                    col.child(
                                        div().text_size(px(11.)).text_color(muted).child(label),
                                    )
                                },
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(px(11.5))
                            .text_color(muted)
                            .child(crate::usage::percent_label(window.used_percent)),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h(px(6.))
                    .rounded_full()
                    .bg(cx.theme().muted)
                    .child(div().h_full().rounded_full().bg(fill).w(gpui::relative(
                        window.used_percent.clamp(0.0, 100.0) / 100.0,
                    ))),
            )
            .into_any_element()
    }

    /// A muted status row inside a usage card ("Checking…", "No usage data
    /// yet", or "Usage unavailable" over the provider's own error text).
    fn usage_note_row(
        &self,
        label: String,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        self.row_frame(cx)
            .flex_col()
            .items_start()
            .gap(px(2.))
            .text_color(muted)
            .child(div().text_size(px(11.5)).child(label))
            .when_some(detail, |col, detail| {
                col.child(div().text_size(px(11.)).child(detail))
            })
            .into_any_element()
    }

    /// Archived Threads: archived sessions grouped by project, each with
    /// Unarchive + Delete-permanently controls (Group A).
    fn render_archived(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let groups = self.store.read(cx).archived_groups();
        let settings = self.store.read(cx).settings();
        let days = settings.auto_archive_max_idle_days.max(1);
        let keep = settings.auto_archive_keep_count.max(1);
        let auto_archive_reset = self.reset_action(
            "reset-auto-archive",
            settings.auto_archive_disabled,
            cx,
            |this, _, cx| {
                this.dispatch_settings(|store| store.set_auto_archive_disabled(false), cx)
            },
        );
        let idle_days_reset = self.reset_action(
            "reset-auto-archive-idle-days",
            settings.auto_archive_max_idle_days != DEFAULT_AUTO_ARCHIVE_MAX_IDLE_DAYS,
            cx,
            |this, window, cx| {
                this.dispatch_settings(
                    |store| {
                        store.set_auto_archive_max_idle_days(DEFAULT_AUTO_ARCHIVE_MAX_IDLE_DAYS)
                    },
                    cx,
                );
                this.auto_archive_idle_input.update(cx, |input, cx| {
                    input.set_value(DEFAULT_AUTO_ARCHIVE_MAX_IDLE_DAYS.to_string(), window, cx)
                });
            },
        );
        let keep_count_reset = self.reset_action(
            "reset-auto-archive-keep-count",
            settings.auto_archive_keep_count != DEFAULT_AUTO_ARCHIVE_KEEP_COUNT,
            cx,
            |this, window, cx| {
                this.dispatch_settings(
                    |store| store.set_auto_archive_keep_count(DEFAULT_AUTO_ARCHIVE_KEEP_COUNT),
                    cx,
                );
                this.auto_archive_keep_input.update(cx, |input, cx| {
                    input.set_value(DEFAULT_AUTO_ARCHIVE_KEEP_COUNT.to_string(), window, cx)
                });
            },
        );
        let rows = vec![
            self.toggle_row(
                "auto-archive",
                crate::tr!("settings.auto_archive.title"),
                crate::tr!(
                    "settings.auto_archive.description",
                    days = days,
                    keep = keep
                ),
                !settings.auto_archive_disabled,
                auto_archive_reset,
                cx,
                |store, checked| store.set_auto_archive_disabled(!checked),
            ),
            self.row_frame(cx)
                .child(self.row_labels(
                    crate::tr!("settings.auto_archive.idle_days"),
                    crate::tr!("settings.auto_archive.idle_days_description"),
                    idle_days_reset,
                    cx,
                ))
                .child(
                    Input::new(&self.auto_archive_idle_input)
                        .w(px(72.))
                        .rounded(crate::material::radius_input()),
                )
                .into_any_element(),
            self.row_frame(cx)
                .child(self.row_labels(
                    crate::tr!("settings.auto_archive.keep_count"),
                    crate::tr!("settings.auto_archive.keep_count_description"),
                    keep_count_reset,
                    cx,
                ))
                .child(
                    Input::new(&self.auto_archive_keep_input)
                        .w(px(72.))
                        .rounded(crate::material::radius_input()),
                )
                .into_any_element(),
        ];
        let controls = v_flex()
            .child(self.section_label(crate::tr!("settings.auto_archive.section"), cx))
            .child(self.grouped_plain(rows, cx));

        if groups.is_empty() {
            return v_flex()
                .gap(px(20.))
                .child(controls)
                .child(self.section_label(crate::tr!("settings.archived_section"), cx))
                .child(
                    v_flex()
                        .py(px(48.))
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_medium()
                                .child(crate::tr!("settings.archived_empty")),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::tr!("settings.archived_empty_desc")),
                        ),
                );
        }

        let now = now_secs();
        let mut key = 0usize;
        // Each project becomes its own grouped list, spaced from the next.
        let mut col = v_flex()
            .gap(px(20.))
            .child(controls)
            .child(self.section_label(crate::tr!("settings.archived_section"), cx));
        for group in groups {
            let mut rows: Vec<AnyElement> = Vec::new();
            for meta in &group.sessions {
                key += 1;
                let archived_at = meta.archived_at.unwrap_or(meta.created_at);
                let archived_when = humanize_ago(now.saturating_sub(archived_at));
                let created_when = humanize_ago(now.saturating_sub(meta.created_at));
                let desc = format!(
                    "{} · {}",
                    crate::tr!("settings.archived_at", when = archived_when),
                    crate::tr!("settings.archived_created", when = created_when),
                );
                let id_unarchive = meta.id.clone();
                let id_delete = meta.id.clone();
                let title = meta.title.clone();
                rows.push(
                    self.row_frame(cx)
                        .child(self.row_labels(meta.title.clone(), desc, None, cx))
                        .child(
                            gpui_base::h_flex()
                                .flex_none()
                                .gap_2()
                                .child(
                                    Button::new(("unarchive", key))
                                        .outline()
                                        .small()
                                        .label(crate::tr!("settings.unarchive"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let id = id_unarchive.clone();
                                            this.store.update(cx, |store, _cx| {
                                                store.unarchive_session(id);
                                            });
                                        })),
                                )
                                .child(
                                    Button::new(("delete-perm", key))
                                        .danger()
                                        .small()
                                        .label(crate::tr!("settings.delete_permanently"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.confirm_delete_archived(
                                                &id_delete, &title, window, cx,
                                            );
                                        })),
                                ),
                        )
                        .into_any_element(),
                );
            }
            col = col.child(
                v_flex()
                    .child(self.section_label(group.project.name.clone(), cx))
                    .child(self.grouped_plain(rows, cx)),
            );
        }
        col
    }

    /// Confirm and permanently delete an archived thread.
    fn confirm_delete_archived(
        &self,
        session_id: &str,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let session_id = session_id.to_string();
        let title = title.to_string();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let alert = alert.bg(cx.theme().popover);
            let store = store.clone();
            let session_id = session_id.clone();
            alert
                .title(crate::tr!("sidebar.delete_title", title = title.clone()))
                .description(crate::tr!("sidebar.delete_description"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("settings.delete_permanently"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    store.update(cx, |store, _cx| {
                        store.delete_session(session_id.clone(), false);
                    });
                    true
                })
        });
    }

    // -- Computer Use & Browser pages --------------------------------------

    fn render_computer_use(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        let enabled_reset = self.reset_action(
            "reset-cu-enabled",
            settings.computer_use.enabled,
            cx,
            |this, _, cx| this.dispatch_settings(|store| store.set_computer_use_enabled(false), cx),
        );
        let allow_input_reset = self.reset_action(
            "reset-cu-allow-input",
            !settings.computer_use.allow_input,
            cx,
            |this, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_allow_input(true), cx)
            },
        );
        let allow_foreground_fallback_reset = self.reset_action(
            "reset-cu-allow-foreground-fallback",
            settings.computer_use.allow_foreground_fallback,
            cx,
            |this, _, cx| {
                this.dispatch_settings(
                    |store| store.set_computer_use_allow_foreground_fallback(false),
                    cx,
                )
            },
        );
        let show_agent_cursor_reset = self.reset_action(
            "reset-cu-show-agent-cursor",
            !settings.computer_use.show_agent_cursor,
            cx,
            |this, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_show_agent_cursor(true), cx)
            },
        );
        let rows = vec![
            self.toggle_row(
                "cu-enabled",
                crate::tr!("computer_use.enable.title"),
                crate::tr!("computer_use.enable.description"),
                settings.computer_use.enabled,
                enabled_reset,
                cx,
                WorkspaceStore::set_computer_use_enabled,
            ),
            self.image_mode_row(settings.computer_use.image_mode, cx),
            self.toggle_row(
                "cu-allow-input",
                crate::tr!("computer_use.allow_input.title"),
                crate::tr!("computer_use.allow_input.description"),
                settings.computer_use.allow_input,
                allow_input_reset,
                cx,
                WorkspaceStore::set_computer_use_allow_input,
            ),
            self.toggle_row(
                "cu-allow-foreground-fallback",
                crate::tr!("computer_use.allow_foreground_fallback.title"),
                crate::tr!("computer_use.allow_foreground_fallback.description"),
                settings.computer_use.allow_foreground_fallback,
                allow_foreground_fallback_reset,
                cx,
                WorkspaceStore::set_computer_use_allow_foreground_fallback,
            ),
            self.toggle_row(
                "cu-show-agent-cursor",
                crate::tr!("computer_use.show_agent_cursor.title"),
                crate::tr!("computer_use.show_agent_cursor.description"),
                settings.computer_use.show_agent_cursor,
                show_agent_cursor_reset,
                cx,
                WorkspaceStore::set_computer_use_show_agent_cursor,
            ),
        ];
        v_flex()
            .gap(px(24.))
            .child(
                v_flex()
                    .child(self.section_label(crate::tr!("computer_use.section"), cx))
                    .child(self.grouped_plain(rows, cx)),
            )
            .child(self.permissions_group(
                &[
                    PermissionKind::Accessibility,
                    PermissionKind::ScreenRecording,
                ],
                cx,
            ))
    }

    fn render_browser(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        let enabled_reset = self.reset_action(
            "reset-browser-enabled",
            !settings.browser.enabled,
            cx,
            |this, _, cx| this.dispatch_settings(|store| store.set_browser_enabled(true), cx),
        );
        let allow_eval_reset = self.reset_action(
            "reset-browser-allow-eval",
            !settings.browser.allow_evaluate,
            cx,
            |this, _, cx| {
                this.dispatch_settings(|store| store.set_browser_allow_evaluate(true), cx)
            },
        );
        let rows = vec![
            self.toggle_row(
                "browser-enabled",
                crate::tr!("browser.enable.title"),
                crate::tr!("browser.enable.description"),
                settings.browser.enabled,
                enabled_reset,
                cx,
                WorkspaceStore::set_browser_enabled,
            ),
            self.home_url_row(cx),
            self.toggle_row(
                "browser-allow-eval",
                crate::tr!("browser.allow_evaluate.title"),
                crate::tr!("browser.allow_evaluate.description"),
                settings.browser.allow_evaluate,
                allow_eval_reset,
                cx,
                WorkspaceStore::set_browser_allow_evaluate,
            ),
        ];
        v_flex().gap(px(24.)).child(
            v_flex()
                .child(self.section_label(crate::tr!("browser.section"), cx))
                .child(self.grouped_plain(rows, cx)),
        )
    }

    fn home_url_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let overridden = self.store.read(cx).settings().browser.home_url.is_some();
        let reset = self.reset_action(
            "reset-browser-home-url",
            overridden,
            cx,
            |this, window, cx| {
                this.dispatch_settings(|store| store.set_browser_home_url(None), cx);
                this.home_url_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            },
        );
        self.row_frame(cx)
            .child(self.row_labels(
                crate::tr!("browser.home_url.title"),
                crate::tr!("browser.home_url.description"),
                reset,
                cx,
            ))
            .child(
                div().w(px(240.)).child(
                    Input::new(&self.home_url_input)
                        .small()
                        .rounded(crate::material::radius_input()),
                ),
            )
            .into_any_element()
    }

    fn image_mode_row(&self, mode: ImageMode, cx: &mut Context<Self>) -> AnyElement {
        let reset = self.reset_action(
            "reset-cu-image-mode",
            mode != ImageMode::Auto,
            cx,
            |this, _, cx| {
                this.dispatch_settings(
                    |store| store.set_computer_use_image_mode(ImageMode::Auto),
                    cx,
                );
            },
        );
        let label = match mode {
            ImageMode::Auto => crate::tr!("computer_use.image_mode.auto"),
            ImageMode::Always => crate::tr!("computer_use.image_mode.always"),
            ImageMode::Never => crate::tr!("computer_use.image_mode.never"),
        };
        let option = |value, label_key: &'static str, desc_key: &'static str| SelectRowOption {
            value,
            id: label_key.into(),
            label: crate::tr!(label_key).into_owned().into(),
            description: Some(crate::tr!(desc_key).into_owned().into()),
            selected: value == mode,
        };
        self.select_row(
            "cu-image-mode-dropdown",
            "cu-image-mode-popover",
            "cu-image-mode-menu",
            260.,
            crate::tr!("computer_use.image_mode.title")
                .into_owned()
                .into(),
            crate::tr!("computer_use.image_mode.description")
                .into_owned()
                .into(),
            label.into_owned().into(),
            vec![
                option(
                    ImageMode::Auto,
                    "computer_use.image_mode.auto",
                    "computer_use.image_mode.auto_desc",
                ),
                option(
                    ImageMode::Always,
                    "computer_use.image_mode.always",
                    "computer_use.image_mode.always_desc",
                ),
                option(
                    ImageMode::Never,
                    "computer_use.image_mode.never",
                    "computer_use.image_mode.never_desc",
                ),
            ],
            reset,
            |mode, page, _, cx| {
                page.update(cx, |page, cx| {
                    page.dispatch_settings(|store| store.set_computer_use_image_mode(mode), cx)
                })
            },
            cx,
        )
    }

    /// The Computer Use "System permissions" group. Non-macOS platforms have
    /// no TCC, so it shows a quiet note instead.
    fn permissions_group(&self, kinds: &[PermissionKind], cx: &mut Context<Self>) -> AnyElement {
        let col =
            v_flex().child(self.section_label(crate::tr!("computer_use.permissions_section"), cx));
        if !cfg!(target_os = "macos") {
            return col
                .child(
                    crate::material::group(cx).child(
                        div()
                            .w_full()
                            .px_3()
                            .py_3()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!("permissions.unsupported")),
                    ),
                )
                .into_any_element();
        }
        let rows: Vec<AnyElement> = kinds
            .iter()
            .map(|kind| self.permission_row(*kind, cx))
            .collect();
        let mut stack = v_flex()
            .w_full()
            .gap_2()
            .child(self.grouped_plain(rows, cx));
        // A fresh Screen Recording grant only takes effect after a restart; offer
        // an explicit relaunch when we've detected one is pending.
        if self.sr_restart_hint && kinds.contains(&PermissionKind::ScreenRecording) {
            stack = stack.child(self.restart_banner(cx));
        }
        col.child(stack).into_any_element()
    }

    fn permission_row(&self, kind: PermissionKind, cx: &mut Context<Self>) -> AnyElement {
        let granted = self.perm_status.granted(kind);
        let grant_action = self.permission_grant_flow.action(kind);
        let grant_label = match grant_action {
            PermissionGrantAction::Request => crate::tr!("permissions.grant"),
            PermissionGrantAction::OpenSettings => crate::tr!("permissions.open_settings"),
        };
        let (name_key, why_key, grant_id, recheck_id) = match kind {
            PermissionKind::Accessibility => (
                "permissions.accessibility.name",
                "permissions.accessibility.why",
                "perm-grant-accessibility",
                "perm-recheck-accessibility",
            ),
            PermissionKind::ScreenRecording => (
                "permissions.screen_recording.name",
                "permissions.screen_recording.why",
                "perm-grant-screen-recording",
                "perm-recheck-screen-recording",
            ),
        };
        let mut controls = gpui_base::h_flex()
            .flex_none()
            .gap_2()
            .items_center()
            .child(self.status_chip(granted, cx));
        if !granted {
            controls = controls
                .child(
                    Button::new(grant_id)
                        .outline()
                        .small()
                        .label(grant_label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.grant_permission(kind, cx);
                        })),
                )
                .child(
                    Button::new(recheck_id)
                        .ghost()
                        .small()
                        .label(crate::tr!("permissions.recheck"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.recheck_permissions(true, cx);
                        })),
                );
        }
        // No reset affordance: the grant lives in the OS, not in settings.json.
        self.row_frame(cx)
            .child(self.row_labels(crate::tr!(name_key), crate::tr!(why_key), None, cx))
            .child(controls)
            .into_any_element()
    }

    fn status_chip(&self, granted: bool, cx: &Context<Self>) -> AnyElement {
        let (bg, fg, label) = if granted {
            (
                cx.theme().success.opacity(0.12),
                cx.theme().success_foreground,
                crate::tr!("permissions.granted"),
            )
        } else {
            (
                cx.theme().warning.opacity(0.12),
                cx.theme().warning_foreground,
                crate::tr!("permissions.missing"),
            )
        };
        crate::material::semantic_chip(label, bg, fg).into_any_element()
    }

    fn restart_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        gpui_base::h_flex()
            .w_full()
            .items_center()
            .gap_3()
            .rounded(crate::material::radius_card())
            .bg(cx.theme().warning.opacity(0.12))
            .px_3()
            .py_2p5()
            .child(
                Icon::new(IconName::Info)
                    .small()
                    .text_color(cx.theme().warning_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .child(crate::tr!("permissions.restart_banner")),
            )
            .child(
                Button::new("perm-relaunch")
                    .outline()
                    .small()
                    .label(crate::tr!("permissions.relaunch"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.relaunch(window, cx);
                    })),
            )
            .into_any_element()
    }

    /// Fire the native prompt first. If the permission remains missing, the
    /// next explicit click opens System Settings as a fallback; doing both at
    /// once races macOS's own consent dialog and duplicates its Open Settings
    /// action.
    fn grant_permission(&mut self, kind: PermissionKind, cx: &mut Context<Self>) {
        match self.permission_grant_flow.advance(kind) {
            PermissionGrantAction::Request => {
                if kind == PermissionKind::ScreenRecording {
                    self.store.update(cx, |store, _cx| {
                        store.write_relaunch_marker("computer_use".into());
                    });
                    self.screen_recording_marker_pending = true;
                }
                let _ = request(kind);
                // Both native request APIs may return before the user has
                // completed the system UI, so this immediate snapshot must not
                // clear the temporary Screen Recording marker.
                self.recheck_permissions(false, cx);
            }
            PermissionGrantAction::OpenSettings => {
                open_settings_pane(kind);
                cx.notify();
            }
        }
    }

    fn recheck_permissions(&mut self, clear_ungranted_marker: bool, cx: &mut Context<Self>) {
        let fresh = permissions::check();
        if clear_ungranted_marker && self.screen_recording_marker_pending && !fresh.screen_recording
        {
            self.store.update(cx, |store, _cx| {
                store.clear_relaunch_marker();
            });
            self.screen_recording_marker_pending = false;
        }
        // A Screen Recording grant that flips on still needs a restart to take
        // effect for the running process, so surface the relaunch affordance.
        if fresh.screen_recording && !self.perm_status.screen_recording {
            self.sr_restart_hint = true;
        }
        self.perm_status = fresh;
        cx.notify();
    }

    fn relaunch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _cx| {
            store.write_relaunch_marker("computer_use".into());
        });
        if let Err(err) = relaunch_app() {
            log::warn!("failed to relaunch tcode: {err}");
            return;
        }
        // Quit through the app's existing quit action; the fresh instance
        // consumes the marker on launch.
        window.dispatch_action(Box::new(Quit), cx);
    }

    // -- row builders -------------------------------------------------------

    /// A group's header: 11px muted caption sitting above its container.
    fn section_label(&self, label: impl Into<SharedString>, cx: &mut Context<Self>) -> AnyElement {
        div()
            .pl_3()
            .pb(px(6.))
            .text_size(px(11.))
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label.into())
            .into_any_element()
    }

    /// Assemble rows into a group with NO dividers — chat separates content
    /// with breathing room, not rules. The default for sparse settings surfaces
    /// (General, Browser, Computer Use, Archived, permissions).
    fn grouped_plain(&self, rows: Vec<AnyElement>, cx: &Context<Self>) -> gpui::Div {
        let mut group = crate::material::group(cx);
        for row in rows {
            group = group.child(row);
        }
        group
    }

    /// Left description block (bold title + muted description). `reset` is the
    /// per-item "restore default" affordance and rides inline right after the
    /// title, so it reads as belonging to that setting rather than to the row's
    /// control on the far right.
    fn row_labels(
        &self,
        title: impl Into<SharedString>,
        desc: impl Into<SharedString>,
        reset: Option<AnyElement>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .child(
                gpui_base::h_flex()
                    .items_center()
                    .gap_1()
                    .child(div().text_size(px(15.)).font_medium().child(title.into()))
                    .children(reset),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(desc.into()),
            )
    }

    /// The per-item "restore default" button: icon-only, ghost, and rendered
    /// only while `overridden` — a row showing its factory value has nothing to
    /// restore, so the affordance would just be noise.
    fn reset_action(
        &self,
        id: &'static str,
        overridden: bool,
        cx: &mut Context<Self>,
        on_reset: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Option<AnyElement> {
        if !overridden {
            return None;
        }
        let label = crate::tr!("settings.reset_item").into_owned();
        Some(
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(IconName::Undo)
                .tooltip(label.clone())
                .aria_label(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    // Toggle rows are clickable as a whole; a click that lands
                    // on this button must not also flip the switch.
                    cx.stop_propagation();
                    on_reset(this, window, cx);
                }))
                .into_any_element(),
        )
    }

    /// A single group row: transparent, ~44px min height, label left / control
    /// right. The group container owns the fill and border.
    fn row_frame(&self, _cx: &Context<Self>) -> gpui::Div {
        gpui_base::h_flex()
            .w_full()
            .min_h(px(44.))
            .px_3()
            // Divider-less cards lean on row air for separation: a touch more
            // vertical padding gives neighbours room to breathe.
            .py_2p5()
            .gap_3()
            .items_center()
    }

    #[allow(clippy::too_many_arguments)]
    fn toggle_row(
        &mut self,
        id: &'static str,
        title: impl Into<SharedString>,
        desc: impl Into<SharedString>,
        checked: bool,
        reset: Option<AnyElement>,
        cx: &mut Context<Self>,
        intent: fn(&mut WorkspaceStore, bool),
    ) -> AnyElement {
        let title = title.into();
        let desc = desc.into();
        // The row is the tab stop, but the inline reset button is one too, so
        // the row owns an explicit handle (`accessible_clickable`'s implicit one
        // cannot be queried) and applies the tab order to it by hand.
        let focus = self
            .toggle_focus
            .entry(id)
            .or_insert_with(|| cx.focus_handle().tab_stop(true).tab_index(0))
            .clone();
        let row_focus = focus.clone();
        crate::material::accessible_clickable(
            self.row_frame(cx),
            SharedString::from(format!("{id}-row")),
            Role::Switch,
            title.clone(),
            cx,
        )
        .track_focus(&focus)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .cursor_pointer()
        // Handle Space during capture so the native event cannot fall through
        // to scrolling/text input before the row's synthesized click runs.
        // Enter continues to use GPUI's standard focused-click behavior.
        .capture_key_down(
            cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                // Capture runs from the root down, so without this guard the row
                // would swallow Space aimed at the reset button inside it.
                if !row_focus.is_focused(window) {
                    return;
                }
                if event.keystroke.key == "space"
                    && !event.is_held
                    && !event.keystroke.modifiers.modified()
                {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.dispatch_settings(|store| intent(store, !checked), cx);
                }
            }),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.dispatch_settings(|store| intent(store, !checked), cx);
        }))
        .child(self.row_labels(title, desc, reset, cx))
        // The Switch is intentionally visual here; the semantic row above
        // owns click, focus, keyboard activation, and the toggled state.
        .child(Switch::new(id).checked(checked))
        .into_any_element()
    }

    fn dropdown_trigger(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        cx: &Context<Self>,
    ) -> Button {
        // Ghost fill (transparent at rest, light tint on hover) over a hairline
        // outline — the same quiet trigger the composer's model picker uses, but
        // bordered so it reads as a dropdown rather than plain text.
        Button::new(id).ghost().outline().compact().child(
            gpui_base::h_flex()
                .w(px(160.))
                .items_center()
                .justify_between()
                .gap_2()
                .text_size(px(13.))
                .child(label.into())
                .child(
                    Icon::new(IconName::ChevronDown)
                        .xsmall()
                        .text_color(cx.theme().muted_foreground),
                ),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn select_row<T: Clone + 'static>(
        &self,
        trigger_id: &'static str,
        popover_id: &'static str,
        menu_id: &'static str,
        menu_width: f32,
        title: SharedString,
        description: SharedString,
        selected_label: SharedString,
        options: Vec<SelectRowOption<T>>,
        reset: Option<AnyElement>,
        on_select: impl Fn(T, &Entity<SettingsPage>, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let trigger = self.dropdown_trigger(trigger_id, selected_label, cx);
        let page = cx.entity();
        let on_select = Rc::new(on_select);
        let menu_label = title.clone();
        let dropdown = crate::material::overlay_popover(popover_id)
            .trigger(trigger)
            .content(move |_, _, cx| {
                v_flex()
                    .id(menu_id)
                    .role(Role::Menu)
                    .aria_label(menu_label.clone())
                    .p_1()
                    .min_w(px(menu_width))
                    .gap_0p5()
                    .children(options.clone().into_iter().map(|option| {
                        let page = page.clone();
                        let popover = cx.entity();
                        let on_select = on_select.clone();
                        let label = option.label.clone();
                        let item = crate::material::accessible_clickable(
                            gpui_base::h_flex(),
                            option.id,
                            Role::MenuItem,
                            label.clone(),
                            cx,
                        )
                        .aria_selected(option.selected)
                        .w_full()
                        .px_2()
                        .gap_2()
                        .rounded(crate::material::radius_button())
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().accent));
                        let item = if let Some(description) = option.description {
                            item.py_1p5().items_start().child(
                                v_flex()
                                    .flex_1()
                                    .gap_0p5()
                                    .child(div().text_size(px(13.)).child(label))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(cx.theme().muted_foreground)
                                            .child(description),
                                    ),
                            )
                        } else {
                            item.py_1()
                                .items_center()
                                .text_size(px(13.))
                                .child(div().flex_1().child(label))
                        };
                        item.when(option.selected, |item| {
                            item.child(Icon::new(IconName::Check).xsmall())
                        })
                        .on_click(move |_, window, cx| {
                            on_select(option.value.clone(), &page, window, cx);
                            popover.update(cx, |state, cx| state.dismiss(window, cx));
                        })
                    }))
            });

        self.row_frame(cx)
            .child(self.row_labels(title, description, reset, cx))
            .child(dropdown)
            .into_any_element()
    }

    fn theme_row(&self, mode: ThemeMode, cx: &mut Context<Self>) -> AnyElement {
        let reset = self.reset_action(
            "reset-theme",
            mode != ThemeMode::System,
            cx,
            |this, window, cx| {
                this.dispatch_settings(|store| store.set_theme_mode(ThemeMode::System), cx);
                apply_theme(ThemeMode::System, window, cx);
            },
        );
        let label = match mode {
            ThemeMode::System => crate::tr!("settings.theme.system"),
            ThemeMode::Light => crate::tr!("settings.theme.light"),
            ThemeMode::Dark => crate::tr!("settings.theme.dark"),
        };
        let option = |value, id: &'static str, label_key: &'static str| SelectRowOption {
            value,
            id: id.into(),
            label: crate::tr!(label_key).into_owned().into(),
            description: None,
            selected: value == mode,
        };
        self.select_row(
            "theme-dropdown",
            "theme-popover",
            "theme-options-menu",
            160.,
            crate::tr!("settings.theme.title").into_owned().into(),
            crate::tr!("settings.theme.description").into_owned().into(),
            label.into_owned().into(),
            vec![
                option(
                    ThemeMode::System,
                    "theme-option-system",
                    "settings.theme.system",
                ),
                option(
                    ThemeMode::Light,
                    "theme-option-light",
                    "settings.theme.light",
                ),
                option(ThemeMode::Dark, "theme-option-dark", "settings.theme.dark"),
            ],
            reset,
            |mode, page, window, cx| {
                page.update(cx, |page, cx| {
                    page.dispatch_settings(|store| store.set_theme_mode(mode), cx)
                });
                apply_theme(mode, window, cx);
            },
            cx,
        )
    }

    fn language_row(&self, language: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let selected = language.map(str::to_owned);
        let reset = self.reset_action("reset-language", language.is_some(), cx, |this, _, cx| {
            this.dispatch_settings(|store| store.set_language(None), cx);
        });
        let label = match language {
            Some(LANGUAGE_ENGLISH) => crate::tr!("settings.language.english"),
            Some(LANGUAGE_SIMPLIFIED_CHINESE) => crate::tr!("settings.language.chinese"),
            _ => crate::tr!("settings.language.system"),
        };
        let option = |value, key: &'static str| SelectRowOption {
            value,
            id: key.into(),
            label: crate::tr!(key).into_owned().into(),
            description: None,
            selected: selected.as_deref() == value,
        };
        self.select_row(
            "language-dropdown",
            "language-popover",
            "language-options-menu",
            160.,
            crate::tr!("settings.language.title").into_owned().into(),
            crate::tr!("settings.language.description")
                .into_owned()
                .into(),
            label.into_owned().into(),
            vec![
                option(None, "settings.language.system"),
                option(Some(LANGUAGE_ENGLISH), "settings.language.english"),
                option(
                    Some(LANGUAGE_SIMPLIFIED_CHINESE),
                    "settings.language.chinese",
                ),
            ],
            reset,
            |language, page, _, cx| {
                page.update(cx, |page, cx| {
                    page.dispatch_settings(
                        |store| store.set_language(language.map(str::to_owned)),
                        cx,
                    )
                })
            },
            cx,
        )
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // No opaque full-page fill: the nav must sit on the same translucent
        // glass canvas the chat sidebar does (its `sidebar` token shows the
        // T0 blur through its own translucency), so navigating chat↔settings
        // never flips the window material. Only the content column is paper.
        gpui_base::h_flex()
            .size_full()
            .text_color(cx.theme().foreground)
            .child(self.render_nav(window, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(crate::material::content_surface(cx))
                    // T1 paper floats above the glass canvas — the same shadow
                    // the chat column carries, so the reading plane is identical.
                    .shadow_sm()
                    .child(self.render_header(window, cx))
                    .child(self.render_content(window, cx)),
            )
    }
}
