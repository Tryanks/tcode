//! Full-page settings route (V2-M6). Replaces the old settings dialog.
//!
//! When [`tcode_runtime::app::Route::Settings`] is active, the whole window shows this
//! page: a left nav (same width as the sidebar) listing sections + a pinned
//! "← Back", and a content column of setting rows (bold title + muted
//! description on the left, a control on the right), matching reference shots
//! 40-settings.png / 41-settings-connections.png.

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    ThemeMode as ComponentThemeMode, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::DialogButtonProps,
    popover::Popover,
    switch::Switch,
    v_flex,
};

use agent::ProviderKind;

use tcode_runtime::app::AppState;

use crate::acp_panel::{AcpAgentCard, AcpPanel};
use crate::orchestrate_settings::OrchestrateSettingsPanel;
use crate::provider_card::ProviderCard;
use crate::settings::{LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE, Settings, ThemeMode};
use crate::time::now_secs;
use crate::window_drag_area;

/// Left inset so branding clears the native macOS traffic lights.
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_INSET: f32 = 74.;
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHT_INSET: f32 = 8.;

/// Width of the settings left-nav column (matches the sidebar width).
const NAV_WIDTH: f32 = 255.;
/// Max width of the settings content column.
const CONTENT_MAX_WIDTH: f32 = 720.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    Providers,
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

pub struct SettingsPage {
    app_state: Entity<AppState>,
    /// One card per provider, in T3's driver order (Codex, then Claude).
    provider_cards: Vec<(ProviderKind, Entity<ProviderCard>)>,
    /// Long-lived state for the modal ACP marketplace and custom form.
    acp_panel: Entity<AcpPanel>,
    /// Editable main-model identities and child-model routing matrix.
    orchestrate_panel: Entity<OrchestrateSettingsPanel>,
    /// Stable entities keep expanded state and lazily-created inputs across rerenders.
    acp_cards: Vec<(String, Entity<AcpAgentCard>)>,
    debug_acp_dialog_pending: bool,
    section: Section,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.observe(&app_state, |_, _, cx| cx.notify())];

        // Screenshot-only: `--debug-settings-section` opens a specific section.
        let section = match app_state.read(cx).debug_settings_section.as_deref() {
            Some("providers") => Section::Providers,
            Some("orchestrate") => Section::Orchestrate,
            Some("archived") => Section::Archived,
            _ => Section::General,
        };
        let acp_panel = cx.new(|cx| AcpPanel::new(app_state.clone(), window, cx));
        let orchestrate_panel =
            cx.new(|cx| OrchestrateSettingsPanel::new(app_state.clone(), window, cx));
        let debug_acp_dialog_pending = app_state.read(cx).debug_acp_dialog;
        let mut page = Self {
            app_state,
            provider_cards: Vec::new(),
            acp_panel,
            orchestrate_panel,
            acp_cards: Vec::new(),
            debug_acp_dialog_pending,
            section,
            _subscriptions: subscriptions,
        };
        page.build_provider_cards(window, cx);
        page.sync_acp_cards(window, cx);
        page
    }

    /// (Re)build the provider cards from current settings — also used after
    /// "Restore defaults", which invalidates every card's inputs.
    fn build_provider_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Screenshot-only: `--debug-provider-expanded <codex|claude>` opens one
        // card's details (clicking the chevron cannot be driven headlessly).
        let expanded = self.app_state.read(cx).debug_provider_expanded.clone();
        self.provider_cards = [ProviderKind::Codex, ProviderKind::ClaudeCode]
            .into_iter()
            .map(|provider| {
                let app_state = self.app_state.clone();
                let open = expanded.as_deref() == Some(crate::settings::provider_key(provider));
                let card = cx.new(|cx| ProviderCard::new(app_state, provider, open, window, cx));
                (provider, card)
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
        window.open_dialog(cx, move |dialog, _, _| {
            let panel = panel.clone();
            dialog
                .w(px(620.))
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
            div()
                .id(id)
                .child(
                    gpui_component::h_flex()
                        .h(px(34.))
                        .items_center()
                        .gap_2()
                        .px_2()
                        .rounded(cx.theme().radius)
                        .cursor_pointer()
                        .when(active, |s| s.bg(cx.theme().sidebar_accent))
                        .hover(|s| s.bg(cx.theme().sidebar_accent))
                        .child(Icon::new(icon).size_4().text_color(fg))
                        .child(
                            div()
                                .text_sm()
                                .when(active, |d| d.font_medium())
                                .text_color(fg)
                                .child(label.clone()),
                        ),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.section = section;
                    cx.notify();
                }))
                .into_any_element()
        };

        v_flex()
            .flex_none()
            .w(px(NAV_WIDTH))
            .h_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
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
                .child(
                    div()
                        .text_sm()
                        .font_bold()
                        .text_color(cx.theme().sidebar_foreground)
                        .child("tcode"),
                ),
            )
            .child(
                v_flex()
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
                div()
                    .flex_none()
                    .border_t_1()
                    .border_color(cx.theme().sidebar_border)
                    .child(
                        gpui_component::h_flex()
                            .id("settings-back")
                            .h(px(44.))
                            .items_center()
                            .gap_2()
                            .px_3()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().sidebar_accent))
                            .text_size(px(13.))
                            .text_color(cx.theme().sidebar_foreground)
                            .child(Icon::new(IconName::ArrowLeft).size_4())
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
        window_drag_area(
            "settings-header-drag",
            gpui_component::h_flex()
                .flex_none()
                .h(px(52.))
                .px_6()
                .items_center()
                .border_b_1()
                .border_color(cx.theme().border),
            window,
            cx,
        )
        .child(
            div()
                .flex_1()
                .text_size(px(16.))
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
                    .child(column.w_full().max_w(px(CONTENT_MAX_WIDTH))),
            )
            .into_any_element()
    }

    fn render_general(&self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.app_state.read(cx).settings.clone();
        v_flex()
            .child(self.section_label(tcode_i18n::tr!("settings.general_section"), cx))
            .child(self.language_row(settings.language.as_deref(), cx))
            .child(self.theme_row(settings.theme_mode, cx))
            .child(self.toggle_row(
                "word-wrap",
                tcode_i18n::tr!("settings.word_wrap.title"),
                tcode_i18n::tr!("settings.word_wrap.description"),
                settings.word_wrap_diffs,
                cx,
                |s, checked| s.word_wrap_diffs = checked,
            ))
            .child(self.toggle_row(
                "delete-confirm",
                tcode_i18n::tr!("settings.delete_confirmation.title"),
                tcode_i18n::tr!("settings.delete_confirmation.description"),
                !settings.skip_delete_confirmation,
                cx,
                |s, checked| s.skip_delete_confirmation = !checked,
            ))
            .child(self.toggle_row(
                "auto-open-task-panel",
                tcode_i18n::tr!("settings.auto_open_task_panel.title"),
                tcode_i18n::tr!("settings.auto_open_task_panel.description"),
                settings.auto_open_task_panel,
                cx,
                |s, checked| s.auto_open_task_panel = checked,
            ))
            .child(self.toggle_row(
                "provider-update-checks",
                tcode_i18n::tr!("settings.provider_updates.title"),
                tcode_i18n::tr!("settings.provider_updates.description"),
                // Stored inverted: checked = enabled.
                !settings.provider_update_checks_disabled,
                cx,
                |s, checked| s.provider_update_checks_disabled = !checked,
            ))
    }

    /// Settings → Providers: native providers and installed ACP agents share one
    /// bordered list. The marketplace lives behind the Add agent dialog.
    fn render_providers(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        self.sync_acp_cards(window, cx);
        let state = self.app_state.read(cx);
        let checked_at = state.providers_checked_at();
        let checking = state.providers_checking();
        let muted = cx.theme().muted_foreground;

        let mut header = gpui_component::h_flex()
            .w_full()
            .pb_2()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(muted)
                    .child(tcode_i18n::tr!("settings.providers_section")),
            );
        if let Some(checked_at) = checked_at {
            let ago = humanize_ago(now_secs().saturating_sub(checked_at));
            header = header.child(
                div()
                    .text_size(px(12.))
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

        let mut list = v_flex()
            .w_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden();
        for (index, (_, card)) in self.provider_cards.iter().enumerate() {
            list = list.child(
                v_flex()
                    .w_full()
                    .items_stretch()
                    .when(index > 0, |d| {
                        d.border_t_1().border_color(cx.theme().border)
                    })
                    .child(card.clone()),
            );
        }
        for (_, card) in &self.acp_cards {
            list = list.child(
                v_flex()
                    .w_full()
                    .items_stretch()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(card.clone()),
            );
        }

        v_flex().child(header).child(list)
    }

    /// Archived Threads: archived sessions grouped by project, each with
    /// Unarchive + Delete-permanently controls (Group A).
    fn render_archived(&self, cx: &mut Context<Self>) -> gpui::Div {
        let groups = self.app_state.read(cx).archived_groups();
        let mut col =
            v_flex().child(self.section_label(tcode_i18n::tr!("settings.archived_section"), cx));

        if groups.is_empty() {
            return col.child(
                v_flex()
                    .py(px(48.))
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(14.))
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
        for group in groups {
            col = col.child(
                div()
                    .pt_4()
                    .pb_1()
                    .text_size(px(12.))
                    .font_semibold()
                    .text_color(cx.theme().foreground)
                    .child(group.project.name.clone()),
            );
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
                col = col.child(
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
                        ),
                );
            }
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

    // -- row builders -------------------------------------------------------

    fn section_label(&self, label: impl Into<SharedString>, cx: &mut Context<Self>) -> AnyElement {
        div()
            .pb_2()
            .text_size(px(11.))
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label.into())
            .into_any_element()
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
            .pr_4()
            .child(div().text_size(px(14.)).font_medium().child(title.into()))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(desc.into()),
            )
    }

    fn row_frame(&self, cx: &Context<Self>) -> gpui::Div {
        gpui_component::h_flex()
            .w_full()
            .py_4()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
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
        self.row_frame(cx)
            .child(self.row_labels(title, desc, cx))
            .child(Switch::new(id).checked(checked).on_click(cx.listener(
                move |this, checked: &bool, _, cx| {
                    let checked = *checked;
                    this.update_settings(|s| mutate(s, checked), cx);
                },
            )))
            .into_any_element()
    }

    fn dropdown_trigger(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        cx: &Context<Self>,
    ) -> Button {
        Button::new(id).outline().compact().child(
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
            .trigger(trigger)
            .content(move |_, _, cx| {
                let this = this.clone();
                let option = |mode: ThemeMode,
                              label: SharedString,
                              selected: bool,
                              this: &Entity<SettingsPage>,
                              cx: &mut Context<gpui_component::popover::PopoverState>|
                 -> AnyElement {
                    let this = this.clone();
                    let popover = cx.entity();
                    gpui_component::h_flex()
                        .id(label.clone())
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
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
                    .p_1()
                    .min_w(px(160.))
                    .gap_0p5()
                    .child(option(
                        ThemeMode::System,
                        tcode_i18n::tr!("settings.theme.system").into_owned().into(),
                        mode == ThemeMode::System,
                        &this,
                        cx,
                    ))
                    .child(option(
                        ThemeMode::Light,
                        tcode_i18n::tr!("settings.theme.light").into_owned().into(),
                        mode == ThemeMode::Light,
                        &this,
                        cx,
                    ))
                    .child(option(
                        ThemeMode::Dark,
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
        let dropdown =
            Popover::new("language-popover")
                .trigger(trigger)
                .content(move |_, _, cx| {
                    let option =
                    |value: Option<&'static str>,
                     key: &'static str,
                     cx: &mut Context<gpui_component::popover::PopoverState>| {
                        let page = page.clone();
                        let popover = cx.entity();
                        let is_selected = selected.as_deref() == value;
                        gpui_component::h_flex()
                            .id(key)
                            .w_full()
                            .px_2()
                            .py_1()
                            .gap_2()
                            .items_center()
                            .rounded(px(6.))
                            .text_size(px(13.))
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().accent))
                            .child(div().flex_1().child(tcode_i18n::tr!(key)))
                            .when(is_selected, |d| {
                                d.child(Icon::new(IconName::Check).xsmall())
                            })
                            .on_click(move |_, window, cx| {
                                page.update(cx, |page, cx| {
                                    page.update_settings(
                                        |s| s.language = value.map(str::to_owned),
                                        cx,
                                    )
                                });
                                popover.update(cx, |state, cx| state.dismiss(window, cx));
                            })
                    };
                    v_flex()
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
        gpui_component::h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_nav(window, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
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
