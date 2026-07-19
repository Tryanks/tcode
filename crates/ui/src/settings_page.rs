//! Full-page settings route (V2-M6). Replaces the old settings dialog.
//!
//! When [`tcode_runtime::app::Route::Settings`] is active, the whole window shows this
//! page: a left nav (same width as the sidebar) listing sections + a pinned
//! "← Back", and a content column of setting rows (bold title + muted
//! description on the left, a control on the right), matching reference shots
//! 40-settings.png / 41-settings-connections.png.

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Toggled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    ThemeMode as ComponentThemeMode, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::DialogButtonProps,
    input::{Input, InputEvent, InputState},
    popover::Popover,
    switch::Switch,
    v_flex,
};

use computer_use_mcp::permissions::{
    self, PermissionKind, PermissionStatus, open_settings_pane, relaunch_app, request,
};
use tcode_runtime::app::AppState;

use crate::acp_panel::{AcpAgentCard, AcpPanel};
use crate::orchestrate_settings::OrchestrateSettingsPanel;
use crate::provider_card::ProviderCard;
use crate::provider_model_picker::ProviderModelPicker;
use crate::settings::{
    ImageMode, LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE, Settings, ThemeMode,
};
use crate::shell::Quit;
use crate::time::now_secs;
use crate::window_drag_area;

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

/// Apply a settings theme mode to the live window (shared with the palette's
/// "Toggle theme" action).
pub(crate) fn apply_theme(mode: ThemeMode, window: &mut Window, cx: &mut App) {
    match mode {
        ThemeMode::Light => Theme::change(ComponentThemeMode::Light, Some(window), cx),
        ThemeMode::Dark => Theme::change(ComponentThemeMode::Dark, Some(window), cx),
        ThemeMode::System => Theme::sync_system_appearance(Some(window), cx),
    }
}

fn apply_toggle_value(settings: &mut Settings, checked: bool, mutate: fn(&mut Settings, bool)) {
    mutate(settings, !checked);
}

pub struct SettingsPage {
    app_state: Entity<AppState>,
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
    debug_acp_dialog_pending: bool,
    section: Section,
    /// Editable "Home URL" for the Browser page; committed on change.
    home_url_input: Entity<InputState>,
    /// Last-known TCC permission snapshot, refreshed when Computer Use becomes
    /// visible and on every explicit Recheck / Grant.
    perm_status: PermissionStatus,
    /// Whether a Screen Recording grant looks pending-restart (a fresh grant
    /// only takes effect after tcode relaunches). Drives the restart banner.
    sr_restart_hint: bool,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title_generation = app_state.read(cx).settings.title_generation.clone();
        let title_model_picker = cx.new(|cx| {
            ProviderModelPicker::selection(
                app_state.clone(),
                "title-model-popover",
                "title-model-dropdown",
                title_generation.provider,
                title_generation.model,
                title_generation.profile_id,
                cx,
            )
        });
        let subscriptions = vec![
            cx.observe(&app_state, |this, _, cx| {
                let selection = this.app_state.read(cx).settings.title_generation.clone();
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

        // Screenshot-only / restart-continuity: `--debug-settings-section` (also
        // reused by the relaunch marker) opens a specific section.
        let section = match app_state.read(cx).debug_settings_section.as_deref() {
            Some("providers") => Section::Providers,
            Some("browser") => Section::Browser,
            Some("computer_use") => Section::ComputerUse,
            Some("orchestrate") => Section::Orchestrate,
            Some("archived") => Section::Archived,
            _ => Section::General,
        };
        let acp_panel = cx.new(|cx| AcpPanel::new(app_state.clone(), window, cx));
        let orchestrate_panel =
            cx.new(|cx| OrchestrateSettingsPanel::new(app_state.clone(), window, cx));
        let debug_acp_dialog_pending = app_state.read(cx).debug_acp_dialog;
        let home_url_value = app_state
            .read(cx)
            .settings
            .browser
            .home_url
            .clone()
            .unwrap_or_default();
        let home_url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tcode_i18n::tr!("browser.home_url.placeholder"))
                .default_value(home_url_value)
        });
        // Refresh the TCC snapshot once as the page mounts. When the page is
        // opened by a post-grant relaunch this is the "automatic recheck" that
        // surfaces the new status immediately.
        let perm_status = permissions::check();
        let mut page = Self {
            app_state,
            provider_cards: Vec::new(),
            acp_panel,
            orchestrate_panel,
            title_model_picker,
            acp_cards: Vec::new(),
            debug_acp_dialog_pending,
            section,
            home_url_input: home_url_input.clone(),
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
        page.build_provider_cards(window, cx);
        page.sync_acp_cards(window, cx);
        page
    }

    /// Persist the Browser "Home URL" field (empty → `None`).
    fn commit_home_url(&self, cx: &mut Context<Self>) {
        let value = self.home_url_input.read(cx).value().trim().to_string();
        let home_url = (!value.is_empty()).then_some(value);
        self.update_settings(move |settings| settings.browser.home_url = home_url, cx);
    }

    /// (Re)build the provider cards from current settings — also used after
    /// "Restore defaults", which invalidates every card's inputs.
    fn build_provider_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Screenshot-only: `--debug-provider-expanded <profile-id>` opens one
        // card's details (clicking the chevron cannot be driven headlessly).
        let expanded = self.app_state.read(cx).debug_provider_expanded.clone();
        let profiles = self.app_state.read(cx).all_profiles();
        self.provider_cards = profiles
            .into_iter()
            .map(|profile| {
                let app_state = self.app_state.clone();
                let open = expanded.as_deref() == Some(profile.id.as_str());
                let kind = profile.kind;
                let id = profile.id.clone();
                let card = cx.new(|cx| ProviderCard::new(app_state, kind, id, open, window, cx));
                (profile.id, card)
            })
            .collect();
    }

    /// Reconcile card entities by installed id, preserving editors for agents
    /// that were not installed or removed since the previous render.
    fn sync_acp_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let installed: Vec<_> = self
            .app_state
            .read(cx)
            .settings
            .installed_acp_agents()
            .into_iter()
            .cloned()
            .collect();
        let mut old = std::mem::take(&mut self.acp_cards);
        self.acp_cards = installed
            .into_iter()
            .map(|agent| {
                let card = old
                    .iter()
                    .position(|(id, _)| id == &agent.id)
                    .map(|index| old.swap_remove(index).1)
                    .unwrap_or_else(|| {
                        let app_state = self.app_state.clone();
                        cx.new(|cx| AcpAgentCard::new(app_state, &agent, window, cx))
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
                .title(tcode_i18n::tr!("providers.acp.add_agent").into_owned())
                .content(move |content, _, _| content.h(px(456.)).child(panel.clone()))
        });
    }

    fn update_settings(&self, mutate: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        self.app_state.update(cx, |state, cx| {
            let mut settings = state.settings.clone();
            mutate(&mut settings);
            state.update_settings(settings, cx);
        });
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
                    gpui_component::h_flex()
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
                    gpui_component::h_flex()
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
                    .aria_label(tcode_i18n::tr!("settings.title"))
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .gap(px(2.))
                    .child(nav_item(
                        self,
                        "settings-nav-general",
                        IconName::Settings,
                        tcode_i18n::tr!("settings.general").into_owned().into(),
                        Section::General,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-providers",
                        IconName::Bot,
                        tcode_i18n::tr!("settings.providers").into_owned().into(),
                        Section::Providers,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-browser",
                        IconName::Globe,
                        tcode_i18n::tr!("settings.browser").into_owned().into(),
                        Section::Browser,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-computer-use",
                        IconName::LayoutDashboard,
                        tcode_i18n::tr!("settings.computer_use").into_owned().into(),
                        Section::ComputerUse,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-orchestrate",
                        IconName::Map,
                        tcode_i18n::tr!("settings.orchestrate").into_owned().into(),
                        Section::Orchestrate,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-archived",
                        IconName::Inbox,
                        tcode_i18n::tr!("settings.archived").into_owned().into(),
                        Section::Archived,
                        cx,
                    )),
            )
            .child(
                div().flex_none().child(
                    crate::material::accessible_clickable(
                        gpui_component::h_flex(),
                        "settings-back",
                        Role::Button,
                        tcode_i18n::tr!("settings.back"),
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
                    .child(tcode_i18n::tr!("settings.back"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.close_settings(cx));
                    })),
                ),
            )
            .into_any_element()
    }

    // -- content ------------------------------------------------------------

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // The 52px strip spans the paper full-width (drag area), but its title
        // and actions ride the same centered 768px column as the content below,
        // the way the chat header aligns with its timeline column.
        window_drag_area(
            "settings-header-drag",
            gpui_component::h_flex()
                .flex_none()
                .h(px(52.))
                .w_full()
                .px_6()
                .justify_center()
                .items_center(),
            window,
            cx,
        )
        .child(
            gpui_component::h_flex()
                .w(px(CONTENT_MAX_WIDTH))
                .max_w_full()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .text_size(px(15.))
                        .font_medium()
                        .child(tcode_i18n::tr!("settings.title")),
                )
                .child(
                    Button::new("restore-defaults")
                        .outline()
                        .small()
                        .icon(IconName::Undo)
                        .label(tcode_i18n::tr!("settings.restore"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_restore(window, cx);
                        })),
                ),
        )
        .into_any_element()
    }

    fn confirm_restore(&self, window: &mut Window, cx: &mut Context<Self>) {
        let app_state = self.app_state.clone();
        let page = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            let page = page.clone();
            alert
                .title(tcode_i18n::tr!("settings.restore_title"))
                .description(tcode_i18n::tr!("settings.restore_description"))
                .button_props(
                    DialogButtonProps::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(tcode_i18n::tr!("settings.restore"))
                        .cancel_text(tcode_i18n::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    app_state.update(cx, |state, cx| state.reset_settings(cx));
                    // Every provider card's inputs now hold stale overrides.
                    page.update(cx, |page, cx| page.build_provider_cards(window, cx));
                    page.update(cx, |page, cx| {
                        let app_state = page.app_state.clone();
                        page.orchestrate_panel =
                            cx.new(|cx| OrchestrateSettingsPanel::new(app_state, window, cx));
                        // The Home URL input now holds a stale override.
                        let home_url = page
                            .app_state
                            .read(cx)
                            .settings
                            .browser
                            .home_url
                            .clone()
                            .unwrap_or_default();
                        page.home_url_input
                            .update(cx, |input, cx| input.set_value(home_url, window, cx));
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
                gpui_component::h_flex()
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
        let settings = self.app_state.read(cx).settings.clone();
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
                tcode_i18n::tr!("settings.delete_confirmation.title"),
                tcode_i18n::tr!("settings.delete_confirmation.description"),
                !settings.skip_delete_confirmation,
                cx,
                |s, checked| s.skip_delete_confirmation = !checked,
            ),
            self.toggle_row(
                "auto-open-task-panel",
                tcode_i18n::tr!("settings.auto_open_task_panel.title"),
                tcode_i18n::tr!("settings.auto_open_task_panel.description"),
                settings.auto_open_task_panel,
                cx,
                |s, checked| s.auto_open_task_panel = checked,
            ),
        ];
        let workspace = vec![
            self.toggle_row(
                "word-wrap",
                tcode_i18n::tr!("settings.word_wrap.title"),
                tcode_i18n::tr!("settings.word_wrap.description"),
                settings.word_wrap_diffs,
                cx,
                |s, checked| s.word_wrap_diffs = checked,
            ),
            self.toggle_row(
                "provider-update-checks",
                tcode_i18n::tr!("settings.provider_updates.title"),
                tcode_i18n::tr!("settings.provider_updates.description"),
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
                    .child(self.section_label(tcode_i18n::tr!("settings.appearance_section"), cx))
                    .child(self.grouped_plain(appearance, cx)),
            )
            .child(
                v_flex()
                    .child(self.section_label(tcode_i18n::tr!("settings.conversation_section"), cx))
                    .child(self.grouped_plain(conversation, cx)),
            )
            .child(
                v_flex()
                    .child(self.section_label(tcode_i18n::tr!("settings.workspace_section"), cx))
                    .child(self.grouped_plain(workspace, cx)),
            )
    }

    fn title_generation_row(&self, cx: &mut Context<Self>) -> AnyElement {
        self.row_frame(cx)
            .child(self.row_labels(
                tcode_i18n::tr!("settings.title_generation.title"),
                tcode_i18n::tr!("settings.title_generation.description"),
                cx,
            ))
            .child(self.title_model_picker.clone())
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
            .app_state
            .read(cx)
            .all_profiles()
            .into_iter()
            .map(|profile| profile.id)
            .collect();
        let card_ids: Vec<String> = self
            .provider_cards
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        if current_ids != card_ids {
            self.build_provider_cards(window, cx);
        }
        let state = self.app_state.read(cx);
        let checked_at = state.providers_checked_at();
        let checking = state.providers_checking();
        let muted = cx.theme().muted_foreground;

        let mut header = gpui_component::h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .pl_3()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(muted)
                    .child(tcode_i18n::tr!("settings.providers_section")),
            );
        if let Some(checked_at) = checked_at {
            let ago = humanize_ago(now_secs().saturating_sub(checked_at));
            header = header.child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(tcode_i18n::tr!("providers.checked", when = ago).into_owned()),
            );
        }
        header = header.child(
            Button::new("add-acp-agent")
                .outline()
                .xsmall()
                .icon(IconName::Plus)
                .label(tcode_i18n::tr!("providers.acp.add_agent").into_owned())
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
                .tooltip(tcode_i18n::tr!("providers.refresh"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.app_state.update(cx, |state, cx| {
                        state.refresh_provider_status(cx);
                        state.check_provider_versions(cx);
                    });
                })),
        );

        // Native providers form one grouped list; each card renders as a row
        // (its expanded editor nests under the row via an inset hairline).
        let provider_rows: Vec<AnyElement> = self
            .provider_cards
            .iter()
            .map(|(_, card)| card.clone().into_any_element())
            .collect();

        let mut section = v_flex()
            .w_full()
            .gap_3()
            .child(header)
            .child(self.grouped(provider_rows, cx));
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
        let groups = self.app_state.read(cx).archived_groups();

        if groups.is_empty() {
            return v_flex()
                .child(self.section_label(tcode_i18n::tr!("settings.archived_section"), cx))
                .child(
                    v_flex()
                        .py(px(48.))
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_medium()
                                .child(tcode_i18n::tr!("settings.archived_empty")),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .child(tcode_i18n::tr!("settings.archived_empty_desc")),
                        ),
                );
        }

        let now = now_secs();
        let mut key = 0usize;
        // Each project becomes its own grouped list, spaced from the next.
        let mut col = v_flex()
            .gap(px(20.))
            .child(self.section_label(tcode_i18n::tr!("settings.archived_section"), cx));
        for group in groups {
            let mut rows: Vec<AnyElement> = Vec::new();
            for meta in &group.sessions {
                key += 1;
                let archived_at = meta.archived_at.unwrap_or(meta.created_at);
                let archived_when = humanize_ago(now.saturating_sub(archived_at));
                let created_when = humanize_ago(now.saturating_sub(meta.created_at));
                let desc = format!(
                    "{} · {}",
                    tcode_i18n::tr!("settings.archived_at", when = archived_when),
                    tcode_i18n::tr!("settings.archived_created", when = created_when),
                );
                let id_unarchive = meta.id.clone();
                let id_delete = meta.id.clone();
                let title = meta.title.clone();
                rows.push(
                    self.row_frame(cx)
                        .child(self.row_labels(meta.title.clone(), desc, cx))
                        .child(
                            gpui_component::h_flex()
                                .flex_none()
                                .gap_2()
                                .child(
                                    Button::new(("unarchive", key))
                                        .outline()
                                        .small()
                                        .label(tcode_i18n::tr!("settings.unarchive"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let id = id_unarchive.clone();
                                            this.app_state.update(cx, |state, cx| {
                                                state.unarchive_session(&id, cx);
                                            });
                                        })),
                                )
                                .child(
                                    Button::new(("delete-perm", key))
                                        .danger()
                                        .small()
                                        .label(tcode_i18n::tr!("settings.delete_permanently"))
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
        let app_state = self.app_state.clone();
        let session_id = session_id.to_string();
        let title = title.to_string();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            let session_id = session_id.clone();
            alert
                .title(tcode_i18n::tr!(
                    "sidebar.delete_title",
                    title = title.clone()
                ))
                .description(tcode_i18n::tr!("sidebar.delete_description"))
                .button_props(
                    DialogButtonProps::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(tcode_i18n::tr!("settings.delete_permanently"))
                        .cancel_text(tcode_i18n::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    app_state.update(cx, |state, cx| {
                        state.delete_session(&session_id, false, cx);
                    });
                    true
                })
        });
    }

    // -- Computer Use & Browser pages --------------------------------------

    fn render_computer_use(&self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.app_state.read(cx).settings.clone();
        let rows = vec![
            self.toggle_row(
                "cu-enabled",
                tcode_i18n::tr!("computer_use.enable.title"),
                tcode_i18n::tr!("computer_use.enable.description"),
                settings.computer_use.enabled,
                cx,
                |s, checked| s.computer_use.enabled = checked,
            ),
            self.image_mode_row(settings.computer_use.image_mode, cx),
            self.toggle_row(
                "cu-allow-input",
                tcode_i18n::tr!("computer_use.allow_input.title"),
                tcode_i18n::tr!("computer_use.allow_input.description"),
                settings.computer_use.allow_input,
                cx,
                |s, checked| s.computer_use.allow_input = checked,
            ),
        ];
        v_flex()
            .gap(px(24.))
            .child(
                v_flex()
                    .child(self.section_label(tcode_i18n::tr!("computer_use.section"), cx))
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
        let settings = self.app_state.read(cx).settings.clone();
        let rows = vec![
            self.toggle_row(
                "browser-enabled",
                tcode_i18n::tr!("browser.enable.title"),
                tcode_i18n::tr!("browser.enable.description"),
                settings.browser.enabled,
                cx,
                |s, checked| s.browser.enabled = checked,
            ),
            self.home_url_row(cx),
            self.toggle_row(
                "browser-allow-eval",
                tcode_i18n::tr!("browser.allow_evaluate.title"),
                tcode_i18n::tr!("browser.allow_evaluate.description"),
                settings.browser.allow_evaluate,
                cx,
                |s, checked| s.browser.allow_evaluate = checked,
            ),
        ];
        v_flex().gap(px(24.)).child(
            v_flex()
                .child(self.section_label(tcode_i18n::tr!("browser.section"), cx))
                .child(self.grouped_plain(rows, cx)),
        )
    }

    fn home_url_row(&self, cx: &mut Context<Self>) -> AnyElement {
        self.row_frame(cx)
            .child(self.row_labels(
                tcode_i18n::tr!("browser.home_url.title"),
                tcode_i18n::tr!("browser.home_url.description"),
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
            ImageMode::Auto => tcode_i18n::tr!("computer_use.image_mode.auto"),
            ImageMode::Always => tcode_i18n::tr!("computer_use.image_mode.always"),
            ImageMode::Never => tcode_i18n::tr!("computer_use.image_mode.never"),
        };
        let trigger = self.dropdown_trigger("cu-image-mode-dropdown", label, cx);
        let this = cx.entity();
        let dropdown = Popover::new("cu-image-mode-popover")
            // T3 overlay contour: one panel surface (popover fill + hairline +
            // shadow_xl at the 14px overlay radius). The content stays transparent
            // so the popup is a single card, not a card nested in the panel.
            .rounded(crate::material::radius_overlay())
            .shadow_xl()
            .trigger(trigger)
            .content(move |_, _, cx| {
                let this = this.clone();
                let option = |m: ImageMode,
                              label_key: &'static str,
                              desc_key: &'static str,
                              this: &Entity<SettingsPage>,
                              cx: &mut Context<gpui_component::popover::PopoverState>|
                 -> AnyElement {
                    let this = this.clone();
                    let popover = cx.entity();
                    crate::material::accessible_clickable(
                        gpui_component::h_flex(),
                        label_key,
                        Role::MenuItem,
                        tcode_i18n::tr!(label_key),
                        cx,
                    )
                    .aria_selected(m == mode)
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .items_start()
                    .rounded(crate::material::radius_button())
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().accent))
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_0p5()
                            .child(div().text_size(px(13.)).child(tcode_i18n::tr!(label_key)))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(tcode_i18n::tr!(desc_key)),
                            ),
                    )
                    .when(m == mode, |d| d.child(Icon::new(IconName::Check).xsmall()))
                    .on_click(move |_, window, cx| {
                        this.update(cx, |page, cx| {
                            page.update_settings(|s| s.computer_use.image_mode = m, cx);
                        });
                        popover.update(cx, |st, cx| st.dismiss(window, cx));
                    })
                    .into_any_element()
                };
                v_flex()
                    .id("cu-image-mode-menu")
                    .role(Role::Menu)
                    .aria_label(tcode_i18n::tr!("computer_use.image_mode.title"))
                    .p_1()
                    .min_w(px(260.))
                    .gap_0p5()
                    .child(option(
                        ImageMode::Auto,
                        "computer_use.image_mode.auto",
                        "computer_use.image_mode.auto_desc",
                        &this,
                        cx,
                    ))
                    .child(option(
                        ImageMode::Always,
                        "computer_use.image_mode.always",
                        "computer_use.image_mode.always_desc",
                        &this,
                        cx,
                    ))
                    .child(option(
                        ImageMode::Never,
                        "computer_use.image_mode.never",
                        "computer_use.image_mode.never_desc",
                        &this,
                        cx,
                    ))
            });
        self.row_frame(cx)
            .child(self.row_labels(
                tcode_i18n::tr!("computer_use.image_mode.title"),
                tcode_i18n::tr!("computer_use.image_mode.description"),
                cx,
            ))
            .child(dropdown)
            .into_any_element()
    }

    /// The Computer Use "System permissions" group. Non-macOS platforms have
    /// no TCC, so it shows a quiet note instead.
    fn permissions_group(&self, kinds: &[PermissionKind], cx: &mut Context<Self>) -> AnyElement {
        let col = v_flex()
            .child(self.section_label(tcode_i18n::tr!("computer_use.permissions_section"), cx));
        if !cfg!(target_os = "macos") {
            return col
                .child(
                    self.group(cx).child(
                        div()
                            .w_full()
                            .px_3()
                            .py_3()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(tcode_i18n::tr!("permissions.unsupported")),
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
        let mut controls = gpui_component::h_flex()
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
                        .label(tcode_i18n::tr!("permissions.grant"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.grant_permission(kind, cx);
                        })),
                )
                .child(
                    Button::new(recheck_id)
                        .ghost()
                        .small()
                        .label(tcode_i18n::tr!("permissions.recheck"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.recheck_permissions(cx);
                        })),
                );
        }
        self.row_frame(cx)
            .child(self.row_labels(tcode_i18n::tr!(name_key), tcode_i18n::tr!(why_key), cx))
            .child(controls)
            .into_any_element()
    }

    fn status_chip(&self, granted: bool, cx: &Context<Self>) -> AnyElement {
        let (bg, fg, label) = if granted {
            (
                cx.theme().success.opacity(0.12),
                cx.theme().success_foreground,
                tcode_i18n::tr!("permissions.granted"),
            )
        } else {
            (
                cx.theme().warning.opacity(0.12),
                cx.theme().warning_foreground,
                tcode_i18n::tr!("permissions.missing"),
            )
        };
        crate::material::semantic_chip(label, bg, fg).into_any_element()
    }

    fn restart_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        gpui_component::h_flex()
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
                    .child(tcode_i18n::tr!("permissions.restart_banner")),
            )
            .child(
                Button::new("perm-relaunch")
                    .outline()
                    .small()
                    .label(tcode_i18n::tr!("permissions.relaunch"))
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
        self.app_state
            .read(cx)
            .write_relaunch_marker("computer_use");
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
        self.app_state
            .read(cx)
            .write_relaunch_marker("computer_use");
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

    /// One grouped-list container: a floating card on the paper plane, in
    /// chat's composer-console idiom — popover fill, a hairline border,
    /// card-radius corners and a soft shadow so it reads as lifted, not a flat
    /// System-Settings box (docs/visual-redesign.md §5.5, 2026-07 revision).
    fn group(&self, cx: &Context<Self>) -> gpui::Div {
        crate::material::floating_card(v_flex().w_full(), cx).overflow_hidden()
    }

    /// Faint inset hairline between two rows — flush right, indented past the
    /// row's left padding, and dropped to 60% so it whispers rather than rules.
    /// Only dense lists (Providers) use it; sparse surfaces separate with air.
    fn row_divider(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .w_full()
            .pl_3()
            .child(div().w_full().h(px(1.)).bg(cx.theme().border.opacity(0.6)))
            .into_any_element()
    }

    /// Assemble rows into a group with faint inset hairlines between neighbours
    /// (never after the last) — for dense lists where rows need a visible
    /// boundary.
    fn grouped(&self, rows: Vec<AnyElement>, cx: &Context<Self>) -> gpui::Div {
        let mut group = self.group(cx);
        let last = rows.len().saturating_sub(1);
        for (index, row) in rows.into_iter().enumerate() {
            group = group.child(row);
            if index != last {
                group = group.child(self.row_divider(cx));
            }
        }
        group
    }

    /// Assemble rows into a group with NO dividers — chat separates content
    /// with breathing room, not rules. The default for sparse settings surfaces
    /// (General, Browser, Computer Use, Archived, permissions).
    fn grouped_plain(&self, rows: Vec<AnyElement>, cx: &Context<Self>) -> gpui::Div {
        let mut group = self.group(cx);
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
        gpui_component::h_flex()
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
                    this.update_settings(
                        |settings| apply_toggle_value(settings, checked, mutate),
                        cx,
                    );
                }
            }),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.update_settings(|settings| apply_toggle_value(settings, checked, mutate), cx);
        }))
        .child(self.row_labels(title, desc, cx))
        // gpui-component 0315556's Switch is still mouse-only. It is
        // intentionally visual here; the semantic row above owns click,
        // focus, keyboard activation, and the toggled state.
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
            gpui_component::h_flex()
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

    fn theme_row(&self, mode: ThemeMode, cx: &mut Context<Self>) -> AnyElement {
        let label = match mode {
            ThemeMode::System => tcode_i18n::tr!("settings.theme.system"),
            ThemeMode::Light => tcode_i18n::tr!("settings.theme.light"),
            ThemeMode::Dark => tcode_i18n::tr!("settings.theme.dark"),
        };
        let trigger = self.dropdown_trigger("theme-dropdown", label, cx);

        let this = cx.entity();
        let dropdown = Popover::new("theme-popover")
            // Single panel surface at the 14px overlay radius (see image_mode_row).
            .rounded(crate::material::radius_overlay())
            .shadow_xl()
            .trigger(trigger)
            .content(move |_, _, cx| {
                let this = this.clone();
                let option = |mode: ThemeMode,
                              id: &'static str,
                              label: SharedString,
                              selected: bool,
                              this: &Entity<SettingsPage>,
                              cx: &mut Context<gpui_component::popover::PopoverState>|
                 -> AnyElement {
                    let this = this.clone();
                    let popover = cx.entity();
                    crate::material::accessible_clickable(
                        gpui_component::h_flex(),
                        id,
                        Role::MenuItem,
                        label.clone(),
                        cx,
                    )
                    .aria_selected(selected)
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .rounded(crate::material::radius_button())
                    .text_size(px(13.))
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().accent))
                    .child(div().flex_1().child(label.clone()))
                    .when(selected, |d| d.child(Icon::new(IconName::Check).xsmall()))
                    .on_click(move |_, window, cx| {
                        this.update(cx, |page, cx| {
                            page.update_settings(|s| s.theme_mode = mode, cx);
                        });
                        apply_theme(mode, window, cx);
                        popover.update(cx, |st, cx| st.dismiss(window, cx));
                    })
                    .into_any_element()
                };
                v_flex()
                    .id("theme-options-menu")
                    .role(Role::Menu)
                    .aria_label(tcode_i18n::tr!("settings.theme.title"))
                    .p_1()
                    .min_w(px(160.))
                    .gap_0p5()
                    .child(option(
                        ThemeMode::System,
                        "theme-option-system",
                        tcode_i18n::tr!("settings.theme.system").into_owned().into(),
                        mode == ThemeMode::System,
                        &this,
                        cx,
                    ))
                    .child(option(
                        ThemeMode::Light,
                        "theme-option-light",
                        tcode_i18n::tr!("settings.theme.light").into_owned().into(),
                        mode == ThemeMode::Light,
                        &this,
                        cx,
                    ))
                    .child(option(
                        ThemeMode::Dark,
                        "theme-option-dark",
                        tcode_i18n::tr!("settings.theme.dark").into_owned().into(),
                        mode == ThemeMode::Dark,
                        &this,
                        cx,
                    ))
            });

        self.row_frame(cx)
            .child(self.row_labels(
                tcode_i18n::tr!("settings.theme.title"),
                tcode_i18n::tr!("settings.theme.description"),
                cx,
            ))
            .child(dropdown)
            .into_any_element()
    }

    fn language_row(&self, language: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let selected = language.map(str::to_owned);
        let label = match language {
            Some(LANGUAGE_ENGLISH) => tcode_i18n::tr!("settings.language.english"),
            Some(LANGUAGE_SIMPLIFIED_CHINESE) => tcode_i18n::tr!("settings.language.chinese"),
            _ => tcode_i18n::tr!("settings.language.system"),
        };
        let trigger = self.dropdown_trigger("language-dropdown", label, cx);
        let page = cx.entity();
        let dropdown = Popover::new("language-popover")
            // Single panel surface at the 14px overlay radius (see image_mode_row).
            .rounded(crate::material::radius_overlay())
            .shadow_xl()
            .trigger(trigger)
            .content(move |_, _, cx| {
                let option =
                    |value: Option<&'static str>,
                     key: &'static str,
                     cx: &mut Context<gpui_component::popover::PopoverState>| {
                        let page = page.clone();
                        let popover = cx.entity();
                        let is_selected = selected.as_deref() == value;
                        crate::material::accessible_clickable(
                            gpui_component::h_flex(),
                            key,
                            Role::MenuItem,
                            tcode_i18n::tr!(key),
                            cx,
                        )
                        .aria_selected(is_selected)
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .rounded(crate::material::radius_button())
                        .text_size(px(13.))
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().accent))
                        .child(div().flex_1().child(tcode_i18n::tr!(key)))
                        .when(is_selected, |d| {
                            d.child(Icon::new(IconName::Check).xsmall())
                        })
                        .on_click(move |_, window, cx| {
                            page.update(cx, |page, cx| {
                                page.update_settings(|s| s.language = value.map(str::to_owned), cx)
                            });
                            popover.update(cx, |state, cx| state.dismiss(window, cx));
                        })
                    };
                v_flex()
                    .id("language-options-menu")
                    .role(Role::Menu)
                    .aria_label(tcode_i18n::tr!("settings.language.title"))
                    .p_1()
                    .min_w(px(160.))
                    .gap_0p5()
                    .child(option(None, "settings.language.system", cx))
                    .child(option(
                        Some(LANGUAGE_ENGLISH),
                        "settings.language.english",
                        cx,
                    ))
                    .child(option(
                        Some(LANGUAGE_SIMPLIFIED_CHINESE),
                        "settings.language.chinese",
                        cx,
                    ))
            });
        self.row_frame(cx)
            .child(self.row_labels(
                tcode_i18n::tr!("settings.language.title"),
                tcode_i18n::tr!("settings.language.description"),
                cx,
            ))
            .child(dropdown)
            .into_any_element()
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.debug_acp_dialog_pending {
            self.debug_acp_dialog_pending = false;
            self.open_acp_dialog(window, cx);
        }
        // No opaque full-page fill: the nav must sit on the same translucent
        // glass canvas the chat sidebar does (its `sidebar` token shows the
        // T0 blur through its own translucency), so navigating chat↔settings
        // never flips the window material. Only the content column is paper.
        gpui_component::h_flex()
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compact relative-time humanizer for the Archived Threads list.
fn humanize_ago(secs: u64) -> String {
    if secs < 60 {
        tcode_i18n::tr!("time.just_now").into_owned()
    } else if secs < 3600 {
        tcode_i18n::tr!("time.minutes_ago", count = secs / 60).into_owned()
    } else if secs < 86_400 {
        tcode_i18n::tr!("time.hours_ago", count = secs / 3600).into_owned()
    } else {
        tcode_i18n::tr!("time.days_ago", count = secs / 86_400).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessible_toggle_activation_applies_the_inverse_setting_value() {
        let mut settings = Settings::default();

        apply_toggle_value(&mut settings, false, |settings, value| {
            settings.word_wrap_diffs = value;
        });
        assert!(settings.word_wrap_diffs);

        apply_toggle_value(&mut settings, true, |settings, value| {
            settings.word_wrap_diffs = value;
        });
        assert!(!settings.word_wrap_diffs);
    }
}
