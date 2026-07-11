//! Full-page settings route (V2-M6). Replaces the old settings dialog.
//!
//! When [`crate::app::Route::Settings`] is active, the whole window shows this
//! page: a left nav (same width as the sidebar) listing sections + a pinned
//! "← Back", and a content column of setting rows (bold title + muted
//! description on the left, a control on the right), matching reference shots
//! 40-settings.png / 41-settings-connections.png.

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    ThemeMode as ComponentThemeMode, WindowExt as _,
    button::{Button, ButtonVariant},
    dialog::DialogButtonProps,
    input::{Input, InputEvent, InputState},
    popover::Popover,
    switch::Switch,
    v_flex,
};

use crate::app::AppState;
use crate::settings::{Settings, ThemeMode};
use crate::ui::window_drag_area;

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
    claude_input: Entity<InputState>,
    codex_input: Entity<InputState>,
    section: Section,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = app_state.read(cx).settings.clone();
        let claude_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("claude (from PATH)");
            input.set_value(path_string(&settings.claude_binary), window, cx);
            input
        });
        let codex_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("codex (from PATH)");
            input.set_value(path_string(&settings.codex_binary), window, cx);
            input
        });

        let subscriptions = vec![
            cx.observe(&app_state, |_, _, cx| cx.notify()),
            cx.subscribe(&claude_input, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_binaries(cx);
                }
            }),
            cx.subscribe(&codex_input, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_binaries(cx);
                }
            }),
        ];

        Self {
            app_state,
            claude_input,
            codex_input,
            section: Section::General,
            _subscriptions: subscriptions,
        }
    }

    // -- persistence helpers ------------------------------------------------

    fn commit_binaries(&self, cx: &mut Context<Self>) {
        let claude = optional_path(&self.claude_input, cx);
        let codex = optional_path(&self.codex_input, cx);
        self.app_state.update(cx, |state, cx| {
            let mut settings = state.settings.clone();
            settings.claude_binary = claude;
            settings.codex_binary = codex;
            state.update_settings(settings, cx);
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
                        label: &'static str,
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
                                .child(label),
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
            .child(window_drag_area(
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
            ))
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
                        "General",
                        Section::General,
                        cx,
                    ))
                    .child(nav_item(
                        self,
                        "settings-nav-providers",
                        IconName::Bot,
                        "Providers",
                        Section::Providers,
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
                            .child("Back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.app_state.update(cx, |state, cx| state.close_settings(cx));
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
                .child("Settings"),
        )
        .child(
            Button::new("restore-defaults")
                .outline()
                .small()
                .icon(IconName::Undo)
                .label("Restore defaults")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.confirm_restore(window, cx);
                })),
        )
        .into_any_element()
    }

    fn confirm_restore(&self, window: &mut Window, cx: &mut Context<Self>) {
        let app_state = self.app_state.clone();
        let claude_input = self.claude_input.clone();
        let codex_input = self.codex_input.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            let claude_input = claude_input.clone();
            let codex_input = codex_input.clone();
            alert
                .title("Restore default settings?")
                .description(
                    "Reset theme, diff, and provider settings to their defaults. \
                     Your projects and threads are not affected.",
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text("Restore defaults")
                        .cancel_text("Cancel")
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    app_state.update(cx, |state, cx| state.reset_settings(cx));
                    claude_input.update(cx, |s, cx| s.set_value("", window, cx));
                    codex_input.update(cx, |s, cx| s.set_value("", window, cx));
                    apply_theme(ThemeMode::System, window, cx);
                    true
                })
        });
    }

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let column = match self.section {
            Section::General => self.render_general(cx),
            Section::Providers => self.render_providers(cx),
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
            .child(self.section_label("GENERAL", cx))
            .child(self.theme_row(settings.theme_mode, cx))
            .child(self.toggle_row(
                "word-wrap",
                "Word wrap in diffs",
                "Wrap long lines in the diff panel by default.",
                settings.word_wrap_diffs,
                cx,
                |s, checked| s.word_wrap_diffs = checked,
            ))
            .child(self.toggle_row(
                "delete-confirm",
                "Delete confirmation",
                "Ask before archiving a thread and its saved conversation.",
                !settings.skip_delete_confirmation,
                cx,
                |s, checked| s.skip_delete_confirmation = !checked,
            ))
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> gpui::Div {
        v_flex()
            .child(self.section_label("PROVIDERS", cx))
            .child(self.input_row(
                "Claude binary path",
                "Path to the `claude` CLI. Leave empty to use the one on your PATH.",
                &self.claude_input.clone(),
                cx,
            ))
            .child(self.input_row(
                "Codex binary path",
                "Path to the `codex` CLI. Leave empty to use the one on your PATH.",
                &self.codex_input.clone(),
                cx,
            ))
    }

    // -- row builders -------------------------------------------------------

    fn section_label(&self, label: &'static str, cx: &mut Context<Self>) -> AnyElement {
        div()
            .pb_2()
            .text_size(px(11.))
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label)
            .into_any_element()
    }

    /// Left description block (bold title + muted description).
    fn row_labels(&self, title: &'static str, desc: &'static str, cx: &Context<Self>) -> gpui::Div {
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .pr_4()
            .child(div().text_size(px(14.)).font_medium().child(title))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(desc),
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
        title: &'static str,
        desc: &'static str,
        checked: bool,
        cx: &mut Context<Self>,
        mutate: fn(&mut Settings, bool),
    ) -> AnyElement {
        self.row_frame(cx)
            .child(self.row_labels(title, desc, cx))
            .child(
                Switch::new(id).checked(checked).on_click(cx.listener(
                    move |this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.update_settings(|s| mutate(s, checked), cx);
                    },
                )),
            )
            .into_any_element()
    }

    fn input_row(
        &self,
        title: &'static str,
        desc: &'static str,
        input: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.row_frame(cx)
            .child(self.row_labels(title, desc, cx))
            .child(div().w(px(280.)).flex_none().child(Input::new(input)))
            .into_any_element()
    }

    fn theme_row(&self, mode: ThemeMode, cx: &mut Context<Self>) -> AnyElement {
        let label = match mode {
            ThemeMode::System => "System default",
            ThemeMode::Light => "Light",
            ThemeMode::Dark => "Dark",
        };
        let muted = cx.theme().muted_foreground;

        let trigger = Button::new("theme-dropdown").outline().compact().child(
            gpui_component::h_flex()
                .w(px(160.))
                .items_center()
                .justify_between()
                .gap_2()
                .text_size(px(13.))
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );

        let this = cx.entity();
        let dropdown = Popover::new("theme-popover").trigger(trigger).content(
            move |_, _, cx| {
                let this = this.clone();
                let option = |mode: ThemeMode,
                              label: &'static str,
                              selected: bool,
                              this: &Entity<SettingsPage>,
                              cx: &mut Context<gpui_component::popover::PopoverState>|
                 -> AnyElement {
                    let this = this.clone();
                    let popover = cx.entity();
                    gpui_component::h_flex()
                        .id(label)
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .text_size(px(13.))
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().accent))
                        .child(div().flex_1().child(label))
                        .when(selected, |d| {
                            d.child(Icon::new(IconName::Check).xsmall())
                        })
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
                    .child(option(ThemeMode::System, "System default", mode == ThemeMode::System, &this, cx))
                    .child(option(ThemeMode::Light, "Light", mode == ThemeMode::Light, &this, cx))
                    .child(option(ThemeMode::Dark, "Dark", mode == ThemeMode::Dark, &this, cx))
            },
        );

        self.row_frame(cx)
            .child(self.row_labels("Theme", "Choose how tcode looks across the app.", cx))
            .child(dropdown)
            .into_any_element()
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(self.render_content(cx)),
            )
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn path_string(path: &Option<std::path::PathBuf>) -> String {
    path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
}

fn optional_path(input: &Entity<InputState>, cx: &App) -> Option<std::path::PathBuf> {
    let value = input.read(cx).value().trim().to_string();
    (!value.is_empty()).then(|| value.into())
}
