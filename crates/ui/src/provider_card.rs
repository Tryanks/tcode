//! One Settings → Providers list row.
//!
//! A compact, non-expanding row: driver glyph, name, `v<version>`,
//! an update icon when a newer CLI exists, the status summary line, a gear button
//! and the enable switch. The row body and the gear both open the per-profile
//! settings dialog, a transactional modal form.

use crate::icon::Icon;
use crate::widgets::TooltipTrigger as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use crate::{
    icon::IconName,
    overlay::{DialogButtons, OverlayExt as _},
    provider_dialog::ProviderDialog,
    provider_status::{EMAIL_SLOT, ProviderSnapshot, StatusDot, redact_email, signed_in_account},
    sizing::Sizable as _,
    store::{TopicKind, WorkspaceStore, observe_store_topics},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        button::{Button, ButtonVariant, ButtonVariants as _},
        switch::Switch,
    },
};
use gpui::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui::{Styled as _, rgb};
use gpui_base::motion::transition;
use gpui_base::{h_flex, v_flex};

use agent::ProviderKind;
use tcode_core::provider_status::AuthStatus;
use tcode_core::settings::builtin_provider_color;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// The card's dialog asked to manage this profile's plugins.
pub struct ManagePlugins(pub String);

pub struct ProviderCard {
    store: Entity<WorkspaceStore>,
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

impl gpui::EventEmitter<ManagePlugins> for ProviderCard {}

impl ProviderCard {
    pub fn new(
        store: Entity<WorkspaceStore>,
        provider: ProviderKind,
        profile_id: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription =
            observe_store_topics(&store, &[TopicKind::Settings, TopicKind::Providers], cx);
        Self {
            store,
            provider,
            profile_id: profile_id.into(),
            email_revealed: false,
            _subscription: subscription,
        }
    }

    /// Open the per-profile settings modal (the transactional editor).
    fn open_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let provider = self.provider;
        let profile_id = self.profile_id.clone();
        let title = self
            .store
            .read(cx)
            .provider_profile_display_name(&profile_id);
        let card = cx.entity().downgrade();
        let manage_id = profile_id.clone();
        let manage_plugins: std::rc::Rc<dyn Fn(&mut gpui::App)> = std::rc::Rc::new(move |cx| {
            _ = card.update(cx, |_, cx| cx.emit(ManagePlugins(manage_id.clone())));
        });
        let dialog = cx.new(|cx| {
            ProviderDialog::new(
                store.clone(),
                provider,
                profile_id.clone(),
                manage_plugins,
                window,
                cx,
            )
        });
        window.open_dialog(cx, move |dlg, _, cx| {
            let content = dialog.clone();
            dlg.title(title.clone())
                .wide()
                .content(move |content_el, _window, _cx| content_el.child(content.clone()))
                .footer(crate::provider_dialog::render_footer(&dialog, cx))
        });
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let provider = self.provider;
        // Update checks target the built-in installation; a custom profile can
        // launch another binary with a different package-manager environment.
        let store = self.store.read(cx);
        let name = store.provider_profile_display_name(&self.profile_id);
        let enabled = store.provider_profile_settings(&self.profile_id).enabled;
        let snapshot = store.provider_profile_snapshot(&self.profile_id);
        let same_account_as = snapshot
            .as_ref()
            .and_then(|snapshot| same_account_profile(store, &self.profile_id, provider, snapshot));
        let summary = crate::provider_status::summarize(
            provider,
            snapshot.as_ref(),
            enabled,
            same_account_as.as_deref(),
        );
        let provider_version = (self.profile_id
            == tcode_core::settings::Settings::builtin_profile_id(provider))
        .then(|| store.provider_version_status(provider))
        .flatten();
        let version = snapshot
            .as_ref()
            .and_then(|s| s.version.clone())
            .or_else(|| provider_version.as_ref().and_then(|v| v.installed.clone()));
        let update_available = provider_version
            .as_ref()
            .is_some_and(|v| v.update_available);
        let glyph = div().flex_none().text_body(false, cx).child(profile_glyph(
            store,
            provider,
            Some(&self.profile_id),
        ));

        let title = h_flex()
            .gap_2()
            .items_center()
            .child(div().text_title(cx).child(name.clone()))
            .when_some(version, |this, version| {
                this.child(
                    div()
                        .text_meta_mono(false, cx)
                        .child(format!("v{}", version.trim_start_matches('v'))),
                )
            });

        // The row body (glyph + text) is the primary "configure" affordance;
        // the update popover, gear and switch trail it as their own controls.
        let body = crate::material::accessible_clickable(
            h_flex(),
            gpui::SharedString::from(format!("configure-{}", self.profile_id)),
            gpui::Role::Button,
            crate::tr!("providers.configure", name = name.clone()),
            cx,
        )
        .flex_1()
        .min_w_0()
        .gap_3()
        .items_center()
        .cursor_pointer()
        .child(glyph)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(title)
                .child(self.render_summary_line(&summary, window, cx))
                .when(!provider.caps().mcp_servers, |this| {
                    this.child(
                        div()
                            .text_secondary(false, cx)
                            .child(crate::tr!("providers.mcp_unavailable")),
                    )
                }),
        )
        .with_tooltip({
            let name = name.clone();
            move |_, _| {
                let label = crate::tr!("providers.configure", name = name.clone()).into_owned();
                crate::widgets::tooltip::Tooltip::new(label)
            }
        })
        .on_click(cx.listener(|this, _, window, cx| this.open_dialog(window, cx)));

        crate::material::group_row(cx)
            .child(body)
            .children(
                crate::motion::shown(
                    "update-available-slot",
                    update_available.then_some(()),
                    SHOW,
                    window,
                    cx,
                )
                .map(|update| update.fade(self.render_update_popover(cx))),
            )
            .child(
                Button::new("configure-profile")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Settings)
                    .tooltip(crate::tr!("providers.configure", name = name.clone()))
                    .on_click(cx.listener(|this, _, window, cx| this.open_dialog(window, cx))),
            )
            .child(
                Switch::new("enable-provider")
                    .checked(enabled)
                    .tooltip(crate::tr!("providers.enable", name = name))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let checked = *checked;
                        let profile_id = this.profile_id.clone();
                        this.store.update(cx, |store, _cx| {
                            store.update_profile_settings(
                                profile_id,
                                tcode_core::settings::ProfileSettingsPatch::SetEnabled {
                                    enabled: checked,
                                },
                            );
                            store.reload_provider();
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let (status_bg, status_fg) = match summary.dot {
            StatusDot::Loading => (cx.theme().muted.opacity(0.), muted),
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
        let settle = || gpui_base::motion::Transition::new(GLIDE);
        let status_bg = transition(("provider-status", "bg"), status_bg, settle(), window, cx);
        let status_fg = transition(("provider-status", "fg"), status_fg, settle(), window, cx);
        let (lead, email) = match &summary.email {
            Some(email) => match summary.headline.split_once(EMAIL_SLOT) {
                Some((prefix, suffix)) => (
                    prefix.trim_end().to_string(),
                    Some((email.clone(), suffix.trim_start().to_string())),
                ),
                None => (
                    summary.headline.clone(),
                    Some((email.clone(), String::new())),
                ),
            },
            None => (summary.headline.clone(), None),
        };
        let revealed = self.email_revealed;
        let card = cx.entity().downgrade();
        let text = crate::motion::cross_fade(
            "provider-status-text",
            (lead, email, summary.detail.clone()),
            CHANGE,
            window,
            cx,
            move |(lead, email, detail), _, _| {
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(lead.clone())
                    .when_some(email.clone(), |line, (email, suffix)| {
                        let shown = if revealed {
                            email
                        } else {
                            redact_email(&email)
                        };
                        let card = card.clone();
                        line.child(
                            Button::new("reveal-email")
                                .text()
                                .xsmall()
                                .label(shown)
                                .tooltip(if revealed {
                                    crate::tr!("providers.hide_email")
                                } else {
                                    crate::tr!("providers.reveal_email")
                                })
                                .on_click(move |_, _, cx| {
                                    // The card body around this line opens the
                                    // editor on click; this press is not for it.
                                    cx.stop_propagation();
                                    _ = card.update(cx, |this, cx| {
                                        this.email_revealed = !this.email_revealed;
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(div().child(suffix))
                    })
                    .when(!detail.is_empty(), |line| {
                        line.child(div().child(format!("· {detail}")))
                    })
                    .into_any_element()
            },
        );
        let line = crate::material::chip_surface(status_bg, status_fg, cx)
            .overflow_hidden()
            .child(text);
        // Row wrapper so the chip left-aligns and hugs its content instead of
        // being stretched to the column width by the surrounding v_flex.
        h_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .items_center()
            .child(line)
            .children(self.render_account_action(cx))
            .into_any_element()
    }

    /// Sign in or out, where Tcode runs the profile's own account sign-in.
    /// Its press stops propagating so the card body does not also open the
    /// editor.
    fn render_account_action(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.store.read(cx);
        if !store.provider_profile_supports_account_login(&self.profile_id) {
            return None;
        }
        let snapshot = store.provider_profile_snapshot(&self.profile_id)?;
        let button = match snapshot.auth.map(|auth| auth.status) {
            Some(AuthStatus::Authenticated) => Button::new("provider-sign-out")
                .ghost()
                .label(crate::tr!("providers.login.sign_out"))
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.confirm_sign_out(window, cx);
                })),
            Some(AuthStatus::Unauthenticated) | None if snapshot.installed => {
                Button::new("provider-sign-in")
                    .label(crate::tr!("providers.login.sign_in"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        crate::provider_login_dialog::open(
                            this.store.clone(),
                            this.provider,
                            this.profile_id.clone(),
                            window,
                            cx,
                        );
                    }))
            }
            _ => return None,
        };
        Some(button.xsmall().into_any_element())
    }

    fn confirm_sign_out(&self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let profile_id = self.profile_id.clone();
        let name = store.read(cx).provider_profile_display_name(&profile_id);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let profile_id = profile_id.clone();
            alert
                .title(
                    crate::tr!("providers.login.sign_out_confirm_title", name = name).into_owned(),
                )
                .description(crate::tr!("providers.login.sign_out_confirm_body"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("providers.login.sign_out"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let profile_id = profile_id.clone();
                    store.update(cx, |store, _| store.logout_provider(profile_id));
                    true
                })
        });
    }

    /// The update-available icon + its popover.
    fn render_update_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let provider = self.provider;
        let version = self.store.read(cx).provider_version_status(provider);
        let updating = version.as_ref().is_some_and(|v| v.updating);
        let requires_terminal = version.as_ref().is_some_and(|v| v.update_requires_terminal);
        let command = version.as_ref().and_then(|v| v.update_command.clone());
        let summary = version
            .as_ref()
            .and_then(|v| v.update_command_summary.clone());
        let store = self.store.clone();

        crate::material::overlay_popover("update-popover", cx)
            .p_3()
            .trigger(
                Button::new("update-available")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/download.svg"))
                    .tooltip(crate::tr!("providers.update_aria")),
            )
            .content(move |_, _, cx| {
                let store = store.clone();
                let command = command.clone();
                // The Popover panel supplies the fill, border, shadow and p_3
                // padding; the pane itself stays transparent (single surface).
                let mut pane = v_flex()
                    .w(px(320.))
                    .gap_2()
                    .child(
                        div()
                            .text_title(cx)
                            .child(crate::tr!("providers.update_title")),
                    )
                    .child(div().text_secondary(false, cx).child(if command.is_none() {
                        crate::tr!("providers.update_unmanaged")
                    } else if requires_terminal {
                        crate::tr!("providers.update_terminal")
                    } else {
                        crate::tr!("providers.update_message")
                    }));
                if command.is_some() && !requires_terminal {
                    pane = pane.child(
                        Button::new("update-now")
                            .primary()
                            .small()
                            .loading(updating)
                            .label(if updating {
                                crate::tr!("providers.updating")
                            } else {
                                crate::tr!("providers.update_now")
                            })
                            .on_click({
                                let store = store.clone();
                                move |_, _, cx| {
                                    store.update(cx, |store, _cx| {
                                        store.update_providers(vec![provider]);
                                    });
                                }
                            }),
                    );
                }
                if let Some(command) = command {
                    let copy = command.clone();
                    pane = pane
                        .child(div().pt_1().text_caption(cx).child(if requires_terminal {
                            crate::tr!("providers.update_command")
                        } else {
                            crate::tr!("providers.update_manual")
                        }))
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .items_center()
                                .rounded(crate::material::radius_input(cx))
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
                                        .text_code(cx)
                                        .child(crate::widgets::fade_text(
                                            summary.clone().unwrap_or_else(|| command.clone()),
                                        )),
                                )
                                .child(
                                    Button::new("copy-command")
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Copy)
                                        .tooltip(crate::tr!("providers.copy_command"))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ));
                                        }),
                                )
                                .context_menu({
                                    let command = command.clone();
                                    move |menu, _, _| {
                                        menu.menu(
                                            crate::tr!("providers.copy_command").into_owned(),
                                            Box::new(CopyText(command.clone())),
                                        )
                                    }
                                }),
                        );
                }
                pane
            })
            .into_any_element()
    }
}

impl Render for ProviderCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w_full().child(self.render_header(window, cx))
    }
}

/// The name of another profile of `provider` signed in to the account this
/// profile's snapshot is.
fn same_account_profile(
    store: &WorkspaceStore,
    profile_id: &str,
    provider: ProviderKind,
    snapshot: &ProviderSnapshot,
) -> Option<String> {
    let email = |snapshot: &ProviderSnapshot| {
        signed_in_account(snapshot)
            .and_then(|auth| auth.email.as_deref())
            .map(|email| email.trim().to_lowercase())
    };
    let account = email(snapshot)?;
    store
        .all_provider_profiles()
        .into_iter()
        .filter(|profile| profile.kind == provider && profile.id != profile_id)
        .find(|profile| {
            store
                .provider_profile_snapshot(&profile.id)
                .is_some_and(|other| email(&other).as_ref() == Some(&account))
        })
        .map(|profile| store.provider_profile_display_name(&profile.id))
}

/// The provider's glyph. Claude's is pre-tinted with its brand color; the
/// others take the text color.
pub fn provider_glyph(provider: ProviderKind) -> Icon {
    let (path, tint) = match provider {
        ProviderKind::ClaudeCode => ("icons/claude.svg", builtin_provider_color(provider)),
        ProviderKind::Codex => ("icons/openai.svg", None),
        ProviderKind::Pi => ("icons/pi.svg", None),
        ProviderKind::OpenCode => ("icons/opencode.svg", None),
        ProviderKind::Cursor => ("icons/cursor.svg", None),
        ProviderKind::Grok => ("icons/grok.svg", None),
        ProviderKind::Acp => return Icon::empty(),
    };
    Icon::empty()
        .path(path)
        .when_some(tint, |icon, tint| icon.text_color(rgb(tint)))
}

/// The glyph a provider profile is drawn with wherever that profile, rather
/// than its CLI, is meant: the kind's [`provider_glyph`], tinted with the
/// accent chosen on the profile's card when it has one. `None` is the kind's
/// built-in profile.
pub fn profile_glyph(
    store: &WorkspaceStore,
    provider: ProviderKind,
    profile_id: Option<&str>,
) -> Icon {
    let profile_id =
        profile_id.unwrap_or_else(|| tcode_core::settings::Settings::builtin_profile_id(provider));
    let glyph = provider_glyph(provider);
    match store.provider_profile_accent(profile_id) {
        Some(accent) => glyph.text_color(rgb(accent)),
        None => glyph,
    }
}
