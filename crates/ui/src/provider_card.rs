//! One Settings → Providers list row.
//!
//! A compact, non-expanding row: driver glyph + status dot, name, `v<version>`,
//! an update icon when a newer CLI exists, the status summary line, a gear button
//! and the enable switch. The row body and the gear both open the per-profile
//! settings [`ProviderDialog`], a transactional modal form.

use gpui::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    popover::Popover,
    switch::Switch,
    v_flex,
};

use agent::ProviderKind;
use tcode_runtime::app::AppState;

use crate::provider_dialog::ProviderDialog;
use crate::provider_status::{EMAIL_SLOT, StatusDot, redact_email};

/// Claude's official Clay brand color from Anthropic's media resources.
pub const CLAUDE_BRAND_COLOR: u32 = 0xD97757;

pub struct ProviderCard {
    app_state: Entity<AppState>,
    /// The protocol this card's profile drives (glyph, shared model catalog /
    /// status / version are all keyed on it).
    provider: ProviderKind,
    /// Which profile this row represents: a built-in native-provider id or a
    /// user profile slug. Status/config/secret lookups key on this.
    profile_id: String,
    /// Whether the account email in the summary line is revealed.
    email_revealed: bool,
    _subscription: Subscription,
}

impl ProviderCard {
    pub fn new(
        app_state: Entity<AppState>,
        provider: ProviderKind,
        profile_id: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&app_state, |_, _, cx| cx.notify());
        Self {
            app_state,
            provider,
            profile_id: profile_id.into(),
            email_revealed: false,
            _subscription: subscription,
        }
    }

    /// Open the per-profile settings modal (the transactional editor).
    fn open_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let app_state = self.app_state.clone();
        let provider = self.provider;
        let profile_id = self.profile_id.clone();
        let title = self.app_state.read(cx).profile_display_name(&profile_id);
        let dialog = cx.new(|cx| {
            ProviderDialog::new(app_state.clone(), provider, profile_id.clone(), window, cx)
        });
        window.open_dialog(cx, move |dlg, window, cx| {
            let content = dialog.clone();
            dlg.title(title.clone())
                .w(px(560.))
                // Opaque panel over the library's translucent default.
                .bg(cx.theme().popover)
                .shadow_xl()
                .content(move |content_el, _window, _cx| content_el.child(content.clone()))
                .footer(crate::provider_dialog::render_footer(&dialog, window, cx))
        });
    }

    // -- rendering ----------------------------------------------------------

    /// The row: glyph + dot, name, version, update icon, summary, gear, switch.
    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let provider = self.provider;
        // Name, enabled state, and probe result all belong to this profile;
        // update-check versions remain shared by protocol kind.
        let name = state.profile_display_name(&self.profile_id);
        let enabled = state.profile_settings(&self.profile_id).enabled;
        let summary = crate::provider_status::summarize(
            provider,
            state.profile_snapshot(&self.profile_id),
            enabled,
        );
        let version = state
            .profile_snapshot(&self.profile_id)
            .and_then(|s| s.version.clone())
            .or_else(|| {
                state
                    .provider_version(provider)
                    .and_then(|v| v.installed.clone())
            });
        let update_available = state
            .provider_version(provider)
            .is_some_and(|v| v.update_available);
        let muted = cx.theme().muted_foreground;
        let accent = state.profile_accent(&self.profile_id);

        let dot_color = match summary.dot {
            StatusDot::Success => cx.theme().success,
            StatusDot::Warning => cx.theme().warning,
            StatusDot::Error => cx.theme().danger,
            StatusDot::Amber => cx.theme().warning,
        };

        let provider_icon = provider_glyph(provider).small();
        let provider_icon = match accent {
            Some(accent) => provider_icon.text_color(accent),
            None => provider_icon,
        };
        let glyph = div()
            .relative()
            .flex_none()
            .size(px(20.))
            .child(provider_icon)
            .child(
                div()
                    .absolute()
                    .left(px(-3.))
                    .top(px(-3.))
                    .size(px(7.))
                    .rounded_full()
                    .bg(dot_color),
            );

        let title = h_flex()
            .gap_2()
            .items_center()
            .child(div().text_size(px(15.)).font_semibold().child(name.clone()))
            .when_some(version, |this, version| {
                this.child(
                    div()
                        .font_family("monospace")
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(format!("v{}", version.trim_start_matches('v'))),
                )
            });

        // The row body (glyph + text) is the primary "configure" affordance;
        // the update popover, gear and switch trail it as their own controls.
        let body = h_flex()
            .id(gpui::SharedString::from(format!(
                "configure-{}",
                self.profile_id
            )))
            .flex_1()
            .min_w_0()
            .gap_3()
            .items_center()
            .cursor_pointer()
            .child(div().flex_none().child(glyph))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(title)
                    .child(self.render_summary_line(&summary, cx)),
            )
            .tooltip({
                let name = name.clone();
                move |window, cx| {
                    let label =
                        tcode_i18n::tr!("providers.configure", name = name.clone()).into_owned();
                    gpui_component::tooltip::Tooltip::new(label).build(window, cx)
                }
            })
            .on_click(cx.listener(|this, _, window, cx| this.open_dialog(window, cx)));

        h_flex()
            .w_full()
            .min_h(px(44.))
            .px_3()
            .py_3()
            .gap_3()
            .items_center()
            .child(body)
            .when(update_available, |this| {
                this.child(self.render_update_popover(cx))
            })
            .child(
                Button::new("configure-profile")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Settings)
                    .tooltip(tcode_i18n::tr!("providers.configure", name = name.clone()))
                    .on_click(cx.listener(|this, _, window, cx| this.open_dialog(window, cx))),
            )
            .child(
                Switch::new("enable-provider")
                    .checked(enabled)
                    .tooltip(tcode_i18n::tr!("providers.enable", name = name))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.app_state.update(cx, |state, cx| {
                            let profile_id = this.profile_id.clone();
                            state.update_profile_settings(
                                &profile_id,
                                move |settings| settings.enabled = checked,
                                cx,
                            );
                            state.reload_provider(this.provider, cx);
                        });
                    })),
            )
            .into_any_element()
    }

    /// The status summary: headline (with a click-to-reveal email when the probe
    /// found one) followed by the probe's diagnostic detail.
    fn render_summary_line(
        &self,
        summary: &crate::provider_status::StatusSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let (status_bg, status_fg) = match summary.dot {
            StatusDot::Success => (
                cx.theme().success.opacity(0.12),
                cx.theme().success_foreground,
            ),
            StatusDot::Warning => (
                cx.theme().warning.opacity(0.12),
                cx.theme().warning_foreground,
            ),
            StatusDot::Error => (
                cx.theme().danger.opacity(0.12),
                cx.theme().danger_foreground,
            ),
            StatusDot::Amber => (cx.theme().muted, muted),
        };
        // A compact metadata chip in chat's idiom (11px, tinted fill, pill
        // radius, tight padding) that hugs its content — not a full-width
        // green/orange bar that reads as web-form validation.
        let mut line = h_flex()
            .flex_none()
            .items_center()
            .gap_1()
            .px_2()
            .py(px(1.))
            .rounded_full()
            .bg(status_bg)
            .text_size(px(11.))
            .font_medium()
            .text_color(status_fg);

        match &summary.email {
            Some(email) => {
                let (prefix, suffix) = summary
                    .headline
                    .split_once(EMAIL_SLOT)
                    .unwrap_or((summary.headline.as_str(), ""));
                let revealed = self.email_revealed;
                let shown = if revealed {
                    email.clone()
                } else {
                    redact_email(email)
                };
                line = line
                    .child(div().child(prefix.trim_end().to_string()))
                    .child(
                        div()
                            .id("reveal-email")
                            .px_1()
                            .rounded(crate::material::radius_button())
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().accent))
                            .child(shown)
                            .tooltip(move |window, cx| {
                                let label = if revealed {
                                    tcode_i18n::tr!("providers.hide_email")
                                } else {
                                    tcode_i18n::tr!("providers.reveal_email")
                                }
                                .into_owned();
                                gpui_component::tooltip::Tooltip::new(label).build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.email_revealed = !this.email_revealed;
                                cx.notify();
                            })),
                    )
                    .child(div().child(suffix.trim_start().to_string()));
            }
            None => line = line.child(div().child(summary.headline.clone())),
        }
        if !summary.detail.is_empty() {
            line = line.child(div().child(format!("· {}", summary.detail)));
        }
        // Row wrapper so the chip left-aligns and hugs its content instead of
        // being stretched to the column width by the surrounding v_flex.
        h_flex().w_full().min_w_0().child(line).into_any_element()
    }

    /// The update-available icon + its popover.
    fn render_update_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let provider = self.provider;
        let version = state.provider_version(provider);
        let updating = version.is_some_and(|v| v.updating);
        let command = state.provider_update_command(provider);
        let app_state = self.app_state.clone();

        Popover::new("update-popover")
            // Single panel surface at the 14px overlay radius; content transparent.
            .rounded(crate::material::radius_overlay())
            .shadow_xl()
            .trigger(
                Button::new("update-available")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/download.svg"))
                    .tooltip(tcode_i18n::tr!("providers.update_aria")),
            )
            .content(move |_, _, cx| {
                let app_state = app_state.clone();
                let command = command.clone();
                let muted = cx.theme().muted_foreground;
                // The Popover panel supplies the fill, border, shadow and p_3
                // padding; the pane itself stays transparent (single surface).
                let mut pane = v_flex()
                    .w(px(320.))
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .child(tcode_i18n::tr!("providers.update_title")),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(muted)
                            .child(tcode_i18n::tr!("providers.update_message")),
                    );
                if command.is_some() {
                    pane = pane.child(
                        Button::new("update-now")
                            .primary()
                            .small()
                            .loading(updating)
                            .label(if updating {
                                tcode_i18n::tr!("providers.updating")
                            } else {
                                tcode_i18n::tr!("providers.update_now")
                            })
                            .on_click({
                                let app_state = app_state.clone();
                                move |_, _, cx| {
                                    app_state.update(cx, |state, cx| {
                                        state.update_provider(provider, cx);
                                    });
                                }
                            }),
                    );
                }
                if let Some(command) = command {
                    let copy = command.clone();
                    pane = pane
                        .child(
                            div()
                                .pt_1()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(tcode_i18n::tr!("providers.update_manual")),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .items_center()
                                .rounded(crate::material::radius_input())
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().muted)
                                .px_2()
                                .py_1()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .font_family("monospace")
                                        .text_size(px(11.))
                                        .child(command.clone()),
                                )
                                .child(
                                    Button::new("copy-command")
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Copy)
                                        .tooltip(tcode_i18n::tr!("providers.copy_command"))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ));
                                        }),
                                ),
                        );
                }
                pane
            })
            .into_any_element()
    }
}

impl Render for ProviderCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A row inside the providers group — the group owns the fill and border.
        v_flex().w_full().child(self.render_header(cx))
    }
}

// ---------------------------------------------------------------------------
// Shared glyph
// ---------------------------------------------------------------------------

/// The provider's glyph (the same asset the composer's picker rail uses).
pub fn provider_glyph(provider: ProviderKind) -> Icon {
    match provider {
        ProviderKind::ClaudeCode => Icon::empty()
            .path("icons/claude.svg")
            .text_color(rgb(CLAUDE_BRAND_COLOR)),
        ProviderKind::Codex => Icon::empty().path("icons/openai.svg"),
        ProviderKind::Pi => Icon::empty().path("icons/pi.svg"),
        ProviderKind::OpenCode => Icon::empty().path("icons/opencode.svg"),
        ProviderKind::Acp => Icon::empty(),
    }
}
