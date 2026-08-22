//! Full-page settings route (V2-M6). Replaces the old settings dialog.
//!
//! When [`crate::Route::Settings`] is active, the whole window shows this
//! page: a left nav (same width as the sidebar) listing sections + a pinned
//! "← Back", and a content column of setting rows (bold title + muted
//! description on the left, a control on the right), matching reference shots
//! 40-settings.png / 41-settings-connections.png.

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
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Toggled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, v_flex};

use computer_use_mcp::permissions::{
    self, PermissionKind, PermissionStatus, open_settings_pane, relaunch_app, request,
};

use crate::acp_panel::{AcpAgentCard, AcpPanel};
use crate::orchestrate_settings::OrchestrateSettingsPanel;
use crate::provider_card::ProviderCard;
use crate::provider_model_picker::ProviderModelPicker;
use crate::settings::{
    ImageMode, LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE, Settings, ThemeMode,
};
use crate::shell::Quit;
use crate::store::WorkspaceStore;
use crate::theme::{self, ActiveTheme as _, ThemeMode as UiThemeMode};
use crate::time::{humanize_ago, now_secs};
use crate::window_caption;
use crate::window_drag_area;
use crate::window_state::WindowState;

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
    /// Stable entities keep expanded state and lazily-created inputs across rerenders.
    acp_cards: Vec<(String, Entity<AcpAgentCard>)>,
    section: Section,
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
            cx.observe(&window_state, |this, _, cx| {
                let window_state = this.window_state.clone();
                if let Some(section) = Self::take_requested_section(&window_state, cx) {
                    this.section = section;
                }
                cx.notify();
            }),
            cx.subscribe(&title_model_picker, |this, _, event, cx| {
                let selected = event.0.clone();
                this.update_settings(
                    move |settings| {
                        settings.title_generation.provider = selected.provider;
                        settings.title_generation.model = selected.id;
                        settings.title_generation.profile_id = selected.profile_id;
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
        let mut page = Self {
            store,
            window_state,
            provider_cards: Vec::new(),
            acp_panel,
            orchestrate_panel,
            title_model_picker,
            acp_cards: Vec::new(),
            section,
            home_url_input: home_url_input.clone(),
            auto_archive_idle_input: auto_archive_idle_input.clone(),
            auto_archive_keep_input: auto_archive_keep_input.clone(),
            perm_status,
            sr_restart_hint: false,
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
        page
    }

    /// Persist the Browser "Home URL" field (empty → `None`).
    fn commit_home_url(&self, cx: &mut Context<Self>) {
        let value = self.home_url_input.read(cx).value().trim().to_string();
        let home_url = (!value.is_empty()).then_some(value);
        self.update_settings(move |settings| settings.browser.home_url = home_url, cx);
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
        self.update_settings(
            move |settings| settings.auto_archive_max_idle_days = days.max(1),
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
        self.update_settings(
            move |settings| settings.auto_archive_keep_count = keep.max(1),
            cx,
        );
    }

    /// (Re)build the provider cards from current settings — also used after
    /// "Restore defaults", which invalidates the cards' cached settings.
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

    fn update_settings(&self, mutate: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, _cx| store.update_settings(mutate));
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
            crate::material::accessible_clickable(div(), id, Role::Tab, label.clone(), cx)
                .aria_selected(active)
                .child(
                    gpui_base::h_flex()
                        // Match the main sidebar thread rows: 30px tall, 13px
                        // label, a tight 6px rounded rect tinted when active and
                        // a neutral hover only when not.
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
                        ),
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
                // The title stretch carries no controls, so it doubles as the
                // window's native drag handle where the platform needs one.
                .child(window_caption::drag_region(
                    div()
                        .flex_1()
                        .text_size(px(15.))
                        .font_medium()
                        .child(crate::tr!("settings.title")),
                ))
                .child(
                    Button::new("restore-defaults")
                        .outline()
                        .small()
                        .icon(IconName::Undo)
                        .label(crate::tr!("settings.restore"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_restore(window, cx);
                        })),
                ),
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
                    // The profile set may have changed; rebuild the rows.
                    page.update(cx, |page, cx| page.build_provider_cards(cx));
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
                        page.auto_archive_idle_input
                            .update(cx, |input, cx| input.set_value("7", window, cx));
                        page.auto_archive_keep_input
                            .update(cx, |input, cx| input.set_value("30", window, cx));
                    });
                    apply_theme(ThemeMode::System, window, cx);
                    true
                })
        });
    }

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let column = match self.section {
            Section::General => self.render_general(cx),
            Section::Providers => self.render_providers(window, cx),
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

    fn render_general(&self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        // One mega-group on empty paper reads generic. Split the rows into three
        // semantic groups (System-Settings rhythm): 20-24px between groups, each
        // under an 11px caption.
        let appearance = vec![
            self.language_row(settings.language.as_deref(), cx),
            self.theme_row(settings.theme_mode, cx),
        ];
        let conversation = vec![
            self.title_generation_row(cx),
            self.toggle_row(
                "delete-confirm",
                crate::tr!("settings.delete_confirmation.title"),
                crate::tr!("settings.delete_confirmation.description"),
                !settings.skip_delete_confirmation,
                cx,
                |s, checked| s.skip_delete_confirmation = !checked,
            ),
            self.toggle_row(
                "auto-open-task-panel",
                crate::tr!("settings.auto_open_task_panel.title"),
                crate::tr!("settings.auto_open_task_panel.description"),
                settings.auto_open_task_panel,
                cx,
                |s, checked| s.auto_open_task_panel = checked,
            ),
        ];
        let workspace = vec![
            self.toggle_row(
                "word-wrap",
                crate::tr!("settings.word_wrap.title"),
                crate::tr!("settings.word_wrap.description"),
                settings.word_wrap_diffs,
                cx,
                |s, checked| s.word_wrap_diffs = checked,
            ),
            self.toggle_row(
                "provider-update-checks",
                crate::tr!("settings.provider_updates.title"),
                crate::tr!("settings.provider_updates.description"),
                // Stored inverted: checked = enabled.
                !settings.provider_update_checks_disabled,
                cx,
                |s, checked| s.provider_update_checks_disabled = !checked,
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
    }

    fn title_generation_row(&self, cx: &mut Context<Self>) -> AnyElement {
        self.row_frame(cx)
            .child(self.row_labels(
                crate::tr!("settings.title_generation.title"),
                crate::tr!("settings.title_generation.description"),
                cx,
            ))
            .child(self.title_model_picker.clone())
            .into_any_element()
    }

    fn render_tcode_update_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.store.read(cx).tcode_update_status();
        let version = status.latest.unwrap_or_default();
        let release_url = status.release_url.unwrap_or_default();

        crate::material::overlay_popover("tcode-update-popover")
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

    /// Archived Threads: archived sessions grouped by project, each with
    /// Unarchive + Delete-permanently controls (Group A).
    fn render_archived(&self, cx: &mut Context<Self>) -> gpui::Div {
        let groups = self.store.read(cx).archived_groups();
        let settings = self.store.read(cx).settings();
        let days = settings.auto_archive_max_idle_days.max(1);
        let keep = settings.auto_archive_keep_count.max(1);
        let controls = v_flex()
            .child(self.section_label(crate::tr!("settings.auto_archive.section"), cx))
            .child(self.grouped_plain(
                vec![
                    self.toggle_row(
                        "auto-archive",
                        crate::tr!("settings.auto_archive.title"),
                        crate::tr!(
                            "settings.auto_archive.description",
                            days = days,
                            keep = keep
                        ),
                        !settings.auto_archive_disabled,
                        cx,
                        |settings, checked| settings.auto_archive_disabled = !checked,
                    ),
                    self.row_frame(cx)
                        .child(self.row_labels(
                            crate::tr!("settings.auto_archive.idle_days"),
                            crate::tr!("settings.auto_archive.idle_days_description"),
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
                            cx,
                        ))
                        .child(
                            Input::new(&self.auto_archive_keep_input)
                                .w(px(72.))
                                .rounded(crate::material::radius_input()),
                        )
                        .into_any_element(),
                ],
                cx,
            ));

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
                        .child(self.row_labels(meta.title.clone(), desc, cx))
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

    fn render_computer_use(&self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        let rows = vec![
            self.toggle_row(
                "cu-enabled",
                crate::tr!("computer_use.enable.title"),
                crate::tr!("computer_use.enable.description"),
                settings.computer_use.enabled,
                cx,
                |s, checked| s.computer_use.enabled = checked,
            ),
            self.image_mode_row(settings.computer_use.image_mode, cx),
            self.toggle_row(
                "cu-allow-input",
                crate::tr!("computer_use.allow_input.title"),
                crate::tr!("computer_use.allow_input.description"),
                settings.computer_use.allow_input,
                cx,
                |s, checked| s.computer_use.allow_input = checked,
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

    fn render_browser(&self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        let rows = vec![
            self.toggle_row(
                "browser-enabled",
                crate::tr!("browser.enable.title"),
                crate::tr!("browser.enable.description"),
                settings.browser.enabled,
                cx,
                |s, checked| s.browser.enabled = checked,
            ),
            self.home_url_row(cx),
            self.toggle_row(
                "browser-allow-eval",
                crate::tr!("browser.allow_evaluate.title"),
                crate::tr!("browser.allow_evaluate.description"),
                settings.browser.allow_evaluate,
                cx,
                |s, checked| s.browser.allow_evaluate = checked,
            ),
        ];
        v_flex().gap(px(24.)).child(
            v_flex()
                .child(self.section_label(crate::tr!("browser.section"), cx))
                .child(self.grouped_plain(rows, cx)),
        )
    }

    fn home_url_row(&self, cx: &mut Context<Self>) -> AnyElement {
        self.row_frame(cx)
            .child(self.row_labels(
                crate::tr!("browser.home_url.title"),
                crate::tr!("browser.home_url.description"),
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
            |mode, page, _, cx| {
                page.update(cx, |page, cx| {
                    page.update_settings(|settings| settings.computer_use.image_mode = mode, cx)
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
                        .label(crate::tr!("permissions.grant"))
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
                            this.recheck_permissions(cx);
                        })),
                );
        }
        self.row_frame(cx)
            .child(self.row_labels(crate::tr!(name_key), crate::tr!(why_key), cx))
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

    /// Persist the restart-continuity marker, then fire the OS prompt and open
    /// the matching System Settings pane. The marker must be written *first*:
    /// macOS may quit tcode from its own "Quit & Reopen" dialog.
    fn grant_permission(&mut self, kind: PermissionKind, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _cx| {
            store.write_relaunch_marker("computer_use".into());
        });
        let _ = request(kind);
        open_settings_pane(kind);
        if kind == PermissionKind::ScreenRecording {
            self.sr_restart_hint = true;
        }
        self.perm_status = permissions::check();
        cx.notify();
    }

    fn recheck_permissions(&mut self, cx: &mut Context<Self>) {
        let fresh = permissions::check();
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

    /// Left description block (bold title + muted description).
    fn row_labels(
        &self,
        title: impl Into<SharedString>,
        desc: impl Into<SharedString>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .child(div().text_size(px(15.)).font_medium().child(title.into()))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(desc.into()),
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

    fn toggle_row(
        &self,
        id: &'static str,
        title: impl Into<SharedString>,
        desc: impl Into<SharedString>,
        checked: bool,
        cx: &mut Context<Self>,
        mutate: fn(&mut Settings, bool),
    ) -> AnyElement {
        let title = title.into();
        let desc = desc.into();
        crate::material::accessible_clickable(
            self.row_frame(cx),
            SharedString::from(format!("{id}-row")),
            Role::Switch,
            title.clone(),
            cx,
        )
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
                if event.keystroke.key == "space"
                    && !event.is_held
                    && !event.keystroke.modifiers.modified()
                {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.update_settings(|settings| mutate(settings, !checked), cx);
                }
            }),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.update_settings(|settings| mutate(settings, !checked), cx);
        }))
        .child(self.row_labels(title, desc, cx))
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
        // Ghost, not outline: transparent at rest (value + muted chevron) with a
        // light tint only on hover — the same quiet trigger the composer's model
        // picker uses. An outlined trigger reads as a card nested inside the
        // already-bordered group.
        Button::new(id).ghost().compact().child(
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
            .child(self.row_labels(title, description, cx))
            .child(dropdown)
            .into_any_element()
    }

    fn theme_row(&self, mode: ThemeMode, cx: &mut Context<Self>) -> AnyElement {
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
            |mode, page, window, cx| {
                page.update(cx, |page, cx| {
                    page.update_settings(|settings| settings.theme_mode = mode, cx)
                });
                apply_theme(mode, window, cx);
            },
            cx,
        )
    }

    fn language_row(&self, language: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let selected = language.map(str::to_owned);
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
            |language, page, _, cx| {
                page.update(cx, |page, cx| {
                    page.update_settings(
                        |settings| settings.language = language.map(str::to_owned),
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
