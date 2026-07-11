//! One Settings → Providers card (T3's `ProviderInstanceCard`).
//!
//! Collapsed: driver glyph + status dot, name, `v<version>`, an update icon when
//! a newer CLI exists, the status summary line, then a chevron + enable switch.
//! Expanded (T3's order): Display name → Accent color → Environment variables →
//! driver fields → Models.

use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    popover::Popover,
    switch::Switch,
    v_flex,
};

use agent::ProviderKind;

use crate::app::AppState;
use crate::provider_models::{ResolvedModel, SlugError, move_target, reorder, validate_slug};
use crate::provider_status::{EMAIL_SLOT, StatusDot, redact_email};
use crate::settings::{ACCENT_PRESETS, EnvVar, ProviderSettings};
use crate::version_check::update_command_string;

/// One environment-variable row's live inputs.
struct EnvRowState {
    name: Entity<InputState>,
    value: Entity<InputState>,
    sensitive: bool,
    /// A sensitive row whose value is already stored in `secrets.json`. Its
    /// value is never read back, so the input shows the "Stored secret"
    /// placeholder until the user types a replacement.
    stored: bool,
}

pub struct ProviderCard {
    app_state: Entity<AppState>,
    provider: ProviderKind,
    expanded: bool,
    /// Whether the account email in the summary line is revealed.
    email_revealed: bool,
    display_name: Entity<InputState>,
    binary: Entity<InputState>,
    home: Entity<InputState>,
    /// Codex: shadow home path. Claude: launch arguments.
    third_field: Entity<InputState>,
    custom_model: Entity<InputState>,
    slug_error: Option<String>,
    env_rows: Vec<EnvRowState>,
    _subscriptions: Vec<Subscription>,
}

impl ProviderCard {
    pub fn new(
        app_state: Entity<AppState>,
        provider: ProviderKind,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = app_state.read(cx).provider_settings(provider);
        let text_input =
            |placeholder: String, value: String, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    let mut input = InputState::new(window, cx).placeholder(placeholder);
                    input.set_value(value, window, cx);
                    input
                })
            };

        let display_name = text_input(
            crate::settings::provider_label(provider).to_string(),
            settings.display_name.clone().unwrap_or_default(),
            window,
            cx,
        );
        let binary = text_input(
            default_binary_name(provider).to_string(),
            path_string(&settings.binary_path),
            window,
            cx,
        );
        let home = text_input(
            home_placeholder(provider).to_string(),
            path_string(&settings.home_path),
            window,
            cx,
        );
        let third_field = text_input(
            third_placeholder(provider).to_string(),
            match provider {
                ProviderKind::Codex => path_string(&settings.shadow_home_path),
                ProviderKind::ClaudeCode | ProviderKind::Acp => {
                    settings.launch_args.clone().unwrap_or_default()
                }
            },
            window,
            cx,
        );
        let custom_model = text_input(
            custom_model_placeholder(provider).to_string(),
            String::new(),
            window,
            cx,
        );

        let mut subscriptions = vec![cx.observe(&app_state, |_, _, cx| cx.notify())];
        for input in [&display_name, &binary, &home, &third_field] {
            subscriptions.push(cx.subscribe(input, |this, _, event, cx| match event {
                InputEvent::Change => this.commit_fields(cx),
                // Re-running the CLI is deferred until the field is committed,
                // so we don't spawn a probe on every keystroke.
                InputEvent::Blur | InputEvent::PressEnter { .. } => {
                    this.commit_fields(cx);
                    let provider = this.provider;
                    this.app_state
                        .update(cx, |state, cx| state.reload_provider(provider, cx));
                }
                _ => {}
            }));
        }
        subscriptions.push(
            cx.subscribe_in(&custom_model, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_custom_model(window, cx);
                }
            }),
        );

        let mut card = Self {
            app_state,
            provider,
            expanded,
            email_revealed: false,
            display_name,
            binary,
            home,
            third_field,
            custom_model,
            slug_error: None,
            env_rows: Vec::new(),
            _subscriptions: subscriptions,
        };
        card.rebuild_env_rows(&settings, window, cx);
        card
    }

    // -- persistence --------------------------------------------------------

    fn settings(&self, cx: &App) -> ProviderSettings {
        self.app_state.read(cx).provider_settings(self.provider)
    }

    fn update(&self, mutate: impl FnOnce(&mut ProviderSettings), cx: &mut Context<Self>) {
        let provider = self.provider;
        self.app_state.update(cx, |state, cx| {
            state.update_provider_settings(provider, mutate, cx);
        });
    }

    fn reload(&self, cx: &mut Context<Self>) {
        let provider = self.provider;
        self.app_state
            .update(cx, |state, cx| state.reload_provider(provider, cx));
    }

    /// Write the four text fields back into settings (called on every keystroke).
    fn commit_fields(&self, cx: &mut Context<Self>) {
        let name = trimmed(&self.display_name, cx);
        let binary = trimmed(&self.binary, cx);
        let home = trimmed(&self.home, cx);
        let third = trimmed(&self.third_field, cx);
        let provider = self.provider;
        self.update(
            move |settings| {
                settings.display_name = name;
                settings.binary_path = binary.map(Into::into);
                settings.home_path = home.map(Into::into);
                match provider {
                    ProviderKind::Codex => settings.shadow_home_path = third.map(Into::into),
                    ProviderKind::ClaudeCode | ProviderKind::Acp => settings.launch_args = third,
                }
            },
            cx,
        );
    }

    // -- environment variables ---------------------------------------------

    /// Rebuild the env-row inputs from persisted settings (on construction and
    /// after an add/remove).
    fn rebuild_env_rows(
        &mut self,
        settings: &ProviderSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.env_rows.clear();
        let mut subscriptions = Vec::new();
        for var in &settings.env {
            let name = cx.new(|cx| {
                let mut input = InputState::new(window, cx)
                    .placeholder(rust_i18n::t!("providers.env.name_placeholder"));
                input.set_value(var.name.clone(), window, cx);
                input
            });
            // A stored secret is never returned to the app: its input starts
            // empty behind the "Stored secret" placeholder.
            let stored = var.sensitive
                && self
                    .app_state
                    .read(cx)
                    .launch_env(self.provider)
                    .env
                    .iter()
                    .any(|(key, _)| key == &var.name);
            let value = cx.new(|cx| {
                let placeholder = if stored {
                    rust_i18n::t!("providers.env.stored_secret")
                } else {
                    rust_i18n::t!("providers.env.value_placeholder")
                };
                let mut input = InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(var.sensitive);
                input.set_value(
                    if var.sensitive {
                        String::new()
                    } else {
                        var.value.clone()
                    },
                    window,
                    cx,
                );
                input
            });
            for input in [&name, &value] {
                subscriptions.push(cx.subscribe(input, |this, _, event, cx| match event {
                    InputEvent::Change => this.commit_env(cx),
                    InputEvent::Blur | InputEvent::PressEnter { .. } => {
                        this.commit_env(cx);
                        this.reload(cx);
                    }
                    _ => {}
                }));
            }
            self.env_rows.push(EnvRowState {
                name,
                value,
                sensitive: var.sensitive,
                stored,
            });
        }
        self._subscriptions.extend(subscriptions);
    }

    /// Persist the env rows: plaintext values inline, sensitive values into
    /// `secrets.json` (leaving the settings value empty).
    fn commit_env(&mut self, cx: &mut Context<Self>) {
        let mut vars: Vec<EnvVar> = Vec::new();
        let mut secrets: Vec<(String, Option<String>)> = Vec::new();
        for row in &self.env_rows {
            let name = row.name.read(cx).value().trim().to_string();
            let value = row.value.read(cx).value().to_string();
            if name.is_empty() {
                // Keep the (empty) row in the UI, but never persist a nameless
                // variable — it cannot be passed to a child anyway.
                vars.push(EnvVar {
                    name,
                    value: String::new(),
                    sensitive: row.sensitive,
                });
                continue;
            }
            if row.sensitive {
                // An empty input means "leave the stored secret as it is"
                // (nothing was typed to replace it).
                if !value.is_empty() {
                    secrets.push((name.clone(), Some(value)));
                }
                vars.push(EnvVar {
                    name,
                    value: String::new(),
                    sensitive: true,
                });
            } else {
                vars.push(EnvVar {
                    name,
                    value,
                    sensitive: false,
                });
            }
        }
        let persisted: Vec<EnvVar> = vars
            .iter()
            .filter(|var| !var.name.is_empty())
            .cloned()
            .collect();
        self.update(move |settings| settings.env = persisted, cx);
        let provider = self.provider;
        self.app_state.update(cx, |state, cx| {
            for (name, value) in &secrets {
                state.set_provider_secret(provider, name, value.as_deref(), cx);
            }
        });
        // The rows whose secret we just wrote now render as "stored".
        for row in self.env_rows.iter_mut() {
            if row.sensitive && !row.value.read(cx).value().is_empty() {
                row.stored = true;
            }
        }
    }

    fn add_env_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut settings = self.settings(cx);
        // T3: new variables default to sensitive.
        settings.env.push(EnvVar {
            name: String::new(),
            value: String::new(),
            sensitive: true,
        });
        let env = settings.env.clone();
        self.update(move |s| s.env = env, cx);
        let settings = self.settings(cx);
        self.rebuild_env_rows(&settings, window, cx);
        cx.notify();
    }

    fn remove_env_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut settings = self.settings(cx);
        if index >= settings.env.len() {
            return;
        }
        let removed = settings.env.remove(index);
        let env = settings.env.clone();
        self.update(move |s| s.env = env, cx);
        if removed.sensitive && !removed.name.is_empty() {
            let provider = self.provider;
            self.app_state.update(cx, |state, cx| {
                state.set_provider_secret(provider, &removed.name, None, cx);
            });
        }
        let settings = self.settings(cx);
        self.rebuild_env_rows(&settings, window, cx);
        self.reload(cx);
        cx.notify();
    }

    fn toggle_env_sensitive(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut settings = self.settings(cx);
        let Some(var) = settings.env.get_mut(index) else {
            return;
        };
        let name = var.name.clone();
        let becoming_sensitive = !var.sensitive;
        var.sensitive = becoming_sensitive;
        // A value that was plaintext moves into secrets.json (and vice versa the
        // value is simply dropped: we cannot read a stored secret back out).
        let plaintext = std::mem::take(&mut var.value);
        let env = settings.env.clone();
        self.update(move |s| s.env = env, cx);
        if !name.is_empty() {
            let provider = self.provider;
            let secret = becoming_sensitive
                .then_some(plaintext)
                .filter(|v| !v.is_empty());
            self.app_state.update(cx, |state, cx| {
                state.set_provider_secret(provider, &name, secret.as_deref(), cx);
            });
        }
        let settings = self.settings(cx);
        self.rebuild_env_rows(&settings, window, cx);
        self.reload(cx);
        cx.notify();
    }

    // -- models -------------------------------------------------------------

    fn add_custom_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.custom_model.read(cx).value().to_string();
        let settings = self.settings(cx);
        let catalog = self.app_state.read(cx).models_for(self.provider).to_vec();
        match validate_slug(&raw, &catalog, &settings) {
            Ok(slug) => {
                self.slug_error = None;
                self.update(move |s| s.custom_models.push(slug), cx);
                self.custom_model
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
            Err(err) => self.slug_error = Some(SlugError::message(&err)),
        }
        cx.notify();
    }

    fn move_model(&self, rows: &[ResolvedModel], index: usize, up: bool, cx: &mut Context<Self>) {
        let Some(target) = move_target(rows, index, up) else {
            return;
        };
        let order = reorder(rows, index, target);
        self.update(move |s| s.model_order = order, cx);
    }

    fn toggle_hidden(&self, id: &str, cx: &mut Context<Self>) {
        let id = id.to_string();
        self.update(
            move |settings| {
                if let Some(pos) = settings.hidden_models.iter().position(|m| m == &id) {
                    settings.hidden_models.remove(pos);
                } else {
                    settings.hidden_models.push(id);
                }
            },
            cx,
        );
    }

    fn remove_custom_model(&self, id: &str, cx: &mut Context<Self>) {
        let id = id.to_string();
        self.update(
            move |settings| {
                settings.custom_models.retain(|m| m != &id);
                settings.hidden_models.retain(|m| m != &id);
                settings.model_order.retain(|m| m != &id);
            },
            cx,
        );
    }

    // -- rendering ----------------------------------------------------------

    /// The collapsed header: glyph + dot, name, version, update icon, summary,
    /// chevron, enable switch.
    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let provider = self.provider;
        let name = state.settings.provider_display_name(provider);
        let summary = state.provider_summary(provider);
        let enabled = state.provider_enabled(provider);
        let version = state
            .provider_snapshot(provider)
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
        let accent = state.provider_accent(provider);

        let dot_color = match summary.dot {
            StatusDot::Success => cx.theme().success,
            StatusDot::Warning => cx.theme().warning,
            StatusDot::Error => cx.theme().danger,
            // T3 renders the disabled dot amber.
            StatusDot::Amber => rgb(0xf59e0b).into(),
        };

        let glyph = div()
            .relative()
            .flex_none()
            .size(px(20.))
            .child(
                provider_glyph(provider)
                    .small()
                    .text_color(accent.map(Into::into).unwrap_or(cx.theme().foreground)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(-3.))
                    .top(px(-3.))
                    .size(px(7.))
                    .rounded_full()
                    .bg(dot_color),
            );

        let mut title = h_flex()
            .gap_2()
            .items_center()
            .child(div().text_size(px(14.)).font_semibold().child(name.clone()))
            .when_some(version, |this, version| {
                this.child(
                    div()
                        .font_family("monospace")
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(format!("v{}", version.trim_start_matches('v'))),
                )
            });
        if update_available {
            title = title.child(self.render_update_popover(cx));
        }

        h_flex()
            .w_full()
            .px_4()
            .py_3()
            .gap_3()
            .items_center()
            .child(div().flex_none().child(glyph))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(title)
                    .child(self.render_summary_line(&summary, cx)),
            )
            .child(
                Button::new("toggle-details")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .tooltip(rust_i18n::t!(
                        "providers.toggle_details",
                        name = name.clone()
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.expanded = !this.expanded;
                        cx.notify();
                    })),
            )
            .child(
                Switch::new("enable-provider")
                    .checked(enabled)
                    .tooltip(rust_i18n::t!("providers.enable", name = name))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.update(move |settings| settings.enabled = checked, cx);
                        this.reload(cx);
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
        let mut line = h_flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .text_size(px(12.))
            .text_color(muted);

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
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().accent))
                            .text_color(cx.theme().foreground)
                            .child(shown)
                            .tooltip(move |window, cx| {
                                let label = if revealed {
                                    rust_i18n::t!("providers.hide_email")
                                } else {
                                    rust_i18n::t!("providers.reveal_email")
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
        line.into_any_element()
    }

    /// The update-available icon + its popover (T3 §3).
    fn render_update_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let provider = self.provider;
        let version = state.provider_version(provider);
        let updating = version.is_some_and(|v| v.updating);
        let source = version.map(|v| v.install_source).unwrap_or_default();
        let command = update_command_string(provider, source);
        let app_state = self.app_state.clone();

        Popover::new("update-popover")
            .trigger(
                Button::new("update-available")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/download.svg"))
                    .tooltip(rust_i18n::t!("providers.update_aria")),
            )
            .content(move |_, _, cx| {
                let app_state = app_state.clone();
                let command = command.clone();
                let muted = cx.theme().muted_foreground;
                let mut pane = v_flex()
                    .w(px(320.))
                    .p_3()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .child(rust_i18n::t!("providers.update_title")),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(muted)
                            .child(rust_i18n::t!("providers.update_message")),
                    );
                if command.is_some() {
                    pane = pane.child(
                        Button::new("update-now")
                            .primary()
                            .small()
                            .loading(updating)
                            .label(if updating {
                                rust_i18n::t!("providers.updating")
                            } else {
                                rust_i18n::t!("providers.update_now")
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
                                .child(rust_i18n::t!("providers.update_manual")),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .items_center()
                                .rounded(px(6.))
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
                                        .tooltip(rust_i18n::t!("providers.copy_command"))
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

    /// A labelled expanded-card block: label, control, muted help text.
    fn field_block(
        &self,
        label: SharedString,
        help: SharedString,
        control: AnyElement,
        cx: &Context<Self>,
    ) -> AnyElement {
        v_flex()
            .w_full()
            .px_4()
            .py_3()
            .gap_1p5()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(div().text_size(px(13.)).font_medium().child(label))
            .child(control)
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(help),
            )
            .into_any_element()
    }

    fn render_accent(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.settings(cx).accent_color;
        let mut swatches = h_flex().gap_2().items_center();
        for (index, preset) in ACCENT_PRESETS.iter().enumerate() {
            let hex = (*preset).to_string();
            let selected = current.as_deref() == Some(*preset);
            let value = u32::from_str_radix(preset.trim_start_matches('#'), 16).unwrap_or(0);
            swatches = swatches.child(
                div()
                    .id(("accent", index))
                    .size(px(22.))
                    .rounded_full()
                    .cursor_pointer()
                    .bg(rgb(value))
                    .when(selected, |s| {
                        s.border_2().border_color(cx.theme().foreground)
                    })
                    .tooltip({
                        let hex = hex.clone();
                        move |window, cx| {
                            let label =
                                rust_i18n::t!("providers.accent_select", color = hex.clone())
                                    .into_owned();
                            gpui_component::tooltip::Tooltip::new(label).build(window, cx)
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let hex = hex.clone();
                        this.update(move |settings| settings.accent_color = Some(hex), cx);
                    })),
            );
        }
        swatches = swatches.child(
            Button::new("accent-clear")
                .ghost()
                .xsmall()
                .icon(IconName::CircleX)
                .tooltip(rust_i18n::t!("providers.accent_clear"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update(|settings| settings.accent_color = None, cx);
                })),
        );
        swatches.into_any_element()
    }

    fn render_env(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let mut block = v_flex()
            .w_full()
            .px_4()
            .py_3()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_medium()
                            .child(rust_i18n::t!("providers.env.title")),
                    )
                    .child(
                        Button::new("env-add")
                            .outline()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label(rust_i18n::t!("providers.env.add"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_env_row(window, cx);
                            })),
                    ),
            );

        if self.env_rows.is_empty() {
            block = block.child(
                div()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(rust_i18n::t!("providers.env.empty_help")),
            );
        } else {
            for (index, row) in self.env_rows.iter().enumerate() {
                block = block.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .child(div().flex_1().min_w_0().child(Input::new(&row.name)))
                        .child(div().flex_1().min_w_0().child(Input::new(&row.value)))
                        .child(
                            Switch::new(("env-sensitive", index))
                                .checked(row.sensitive)
                                .tooltip(rust_i18n::t!("providers.env.sensitive"))
                                .on_click(cx.listener(move |this, _: &bool, window, cx| {
                                    this.toggle_env_sensitive(index, window, cx);
                                })),
                        )
                        .child(
                            Button::new(("env-remove", index))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Delete)
                                .tooltip(rust_i18n::t!("providers.env.remove"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.remove_env_row(index, window, cx);
                                })),
                        ),
                );
            }
        }
        block
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(rust_i18n::t!("providers.env.security_help")),
            )
            .into_any_element()
    }

    fn render_models(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let rows = state.resolved_models(self.provider);
        let muted = cx.theme().muted_foreground;

        let mut block =
            v_flex()
                .w_full()
                .px_4()
                .py_3()
                .gap_1()
                .border_t_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_size(px(13.))
                        .font_medium()
                        .child(rust_i18n::t!("providers.models.title")),
                )
                .child(div().pb_1().text_size(px(12.)).text_color(muted).child(
                    if rows.len() == 1 {
                        rust_i18n::t!("providers.models.count_one", count = 1).into_owned()
                    } else {
                        rust_i18n::t!("providers.models.count", count = rows.len()).into_owned()
                    },
                ));

        for (index, row) in rows.iter().enumerate() {
            block = block.child(self.render_model_row(&rows, index, row, cx));
        }

        // Custom-model input + validation copy.
        block = block.child(
            h_flex()
                .w_full()
                .pt_2()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&self.custom_model)),
                )
                .child(
                    Button::new("add-custom-model")
                        .outline()
                        .small()
                        .icon(IconName::Plus)
                        .label(rust_i18n::t!("providers.models.add"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_custom_model(window, cx)),
                        ),
                ),
        );
        if let Some(error) = &self.slug_error {
            block = block.child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        block.into_any_element()
    }

    fn render_model_row(
        &self,
        rows: &[ResolvedModel],
        index: usize,
        row: &ResolvedModel,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let name = row.name.clone();
        let hidden = row.hidden;
        let capabilities = row.capabilities.join(" · ");
        let rows_up = rows.to_vec();
        let rows_down = rows.to_vec();
        let can_up = move_target(rows, index, true).is_some();
        let can_down = move_target(rows, index, false).is_some();

        let mut tags = h_flex().gap_1().items_center();
        if row.custom {
            tags = tags.child(tag(
                rust_i18n::t!("providers.models.custom").into_owned(),
                cx,
            ));
        }
        if hidden {
            tags = tags.child(tag(
                rust_i18n::t!("providers.models.hidden").into_owned(),
                cx,
            ));
        }

        let fav_id = row.id.clone();
        let is_fav = row.favorite;
        let app_state = self.app_state.clone();
        let hide_id = row.id.clone();
        let remove_id = row.id.clone();
        let custom = row.custom;

        h_flex()
            .w_full()
            .py_1()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(13.))
                            .when(hidden, |d| d.text_color(muted))
                            .child(name.clone()),
                    )
                    .when(!capabilities.is_empty(), |this| {
                        this.child(
                            div()
                                .id(("model-info", index))
                                .flex_none()
                                .child(Icon::new(IconName::Info).xsmall().text_color(muted))
                                .tooltip(move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(capabilities.clone())
                                        .build(window, cx)
                                }),
                        )
                    })
                    .child(tags),
            )
            .child(
                Button::new(("model-fav", index))
                    .ghost()
                    .xsmall()
                    .icon(if is_fav {
                        IconName::StarFill
                    } else {
                        IconName::Star
                    })
                    .tooltip(if is_fav {
                        rust_i18n::t!("providers.models.unfavorite")
                    } else {
                        rust_i18n::t!("providers.models.favorite")
                    })
                    .on_click(move |_, _, cx| {
                        app_state.update(cx, |state, cx| state.toggle_favorite_model(&fav_id, cx));
                    }),
            )
            .child(
                Button::new(("model-up", index))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowUp)
                    .disabled(!can_up)
                    .tooltip(rust_i18n::t!("providers.models.move_up"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.move_model(&rows_up, index, true, cx);
                    })),
            )
            .child(
                Button::new(("model-down", index))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowDown)
                    .disabled(!can_down)
                    .tooltip(rust_i18n::t!("providers.models.move_down"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.move_model(&rows_down, index, false, cx);
                    })),
            )
            .child(
                Button::new(("model-hide", index))
                    .ghost()
                    .xsmall()
                    .icon(if hidden {
                        IconName::Eye
                    } else {
                        IconName::EyeOff
                    })
                    .tooltip(if hidden {
                        rust_i18n::t!("providers.models.show")
                    } else {
                        rust_i18n::t!("providers.models.hide")
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_hidden(&hide_id, cx))),
            )
            .when(custom, |this| {
                this.child(
                    Button::new(("model-remove", index))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Delete)
                        .tooltip(rust_i18n::t!("providers.models.remove"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_custom_model(&remove_id, cx);
                        })),
                )
            })
            .into_any_element()
    }

    fn render_details(&self, cx: &mut Context<Self>) -> AnyElement {
        let provider = self.provider;
        v_flex()
            .w_full()
            .child(
                self.field_block(
                    rust_i18n::t!("providers.display_name").into_owned().into(),
                    rust_i18n::t!("providers.display_name_help")
                        .into_owned()
                        .into(),
                    Input::new(&self.display_name).into_any_element(),
                    cx,
                ),
            )
            .child(self.field_block(
                rust_i18n::t!("providers.accent").into_owned().into(),
                rust_i18n::t!("providers.accent_help").into_owned().into(),
                self.render_accent(cx),
                cx,
            ))
            .child(self.render_env(cx))
            .child(
                self.field_block(
                    rust_i18n::t!("providers.binary_path").into_owned().into(),
                    rust_i18n::t!(
                        "providers.binary_path_help",
                        name = crate::settings::provider_label(provider)
                    )
                    .into_owned()
                    .into(),
                    Input::new(&self.binary).into_any_element(),
                    cx,
                ),
            )
            .child(self.field_block(
                home_label(provider).into(),
                home_help(provider).into(),
                Input::new(&self.home).into_any_element(),
                cx,
            ))
            .child(self.field_block(
                third_label(provider).into(),
                third_help(provider).into(),
                Input::new(&self.third_field).into_any_element(),
                cx,
            ))
            .child(self.render_models(cx))
            .into_any_element()
    }
}

impl Render for ProviderCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w_full()
            .child(self.render_header(cx))
            .when(self.expanded, |this| this.child(self.render_details(cx)))
    }
}

// ---------------------------------------------------------------------------
// Copy + helpers
// ---------------------------------------------------------------------------

fn tag(label: String, cx: &Context<ProviderCard>) -> AnyElement {
    div()
        .flex_none()
        .px_1()
        .rounded(px(4.))
        .border_1()
        .border_color(cx.theme().border)
        .text_size(px(10.))
        .text_color(cx.theme().muted_foreground)
        .child(label)
        .into_any_element()
}

/// The provider's glyph (the same asset the composer's picker rail uses).
pub fn provider_glyph(provider: ProviderKind) -> Icon {
    match provider {
        ProviderKind::ClaudeCode => Icon::empty().path("icons/claude.svg"),
        ProviderKind::Codex => Icon::empty().path("icons/openai.svg"),
        ProviderKind::Acp => Icon::empty().path("icons/box.svg"),
    }
}

fn default_binary_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "codex",
        ProviderKind::ClaudeCode => "claude",
        // ACP agents are not configured through this card (they have their own
        // marketplace cards); these arms only keep the matches total.
        ProviderKind::Acp => "",
    }
}

fn home_placeholder(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "~/.codex",
        ProviderKind::ClaudeCode | ProviderKind::Acp => "~",
    }
}

fn home_label(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => rust_i18n::t!("providers.codex_home").into_owned(),
        ProviderKind::ClaudeCode | ProviderKind::Acp => {
            rust_i18n::t!("providers.claude_home").into_owned()
        }
    }
}

fn home_help(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => rust_i18n::t!("providers.codex_home_help").into_owned(),
        ProviderKind::ClaudeCode | ProviderKind::Acp => {
            rust_i18n::t!("providers.claude_home_help").into_owned()
        }
    }
}

fn third_placeholder(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "~/.codex-t3/personal",
        ProviderKind::ClaudeCode | ProviderKind::Acp => "e.g. --chrome",
    }
}

fn third_label(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => rust_i18n::t!("providers.shadow_home").into_owned(),
        ProviderKind::ClaudeCode | ProviderKind::Acp => {
            rust_i18n::t!("providers.launch_args").into_owned()
        }
    }
}

fn third_help(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => rust_i18n::t!("providers.shadow_home_help").into_owned(),
        ProviderKind::ClaudeCode | ProviderKind::Acp => {
            rust_i18n::t!("providers.launch_args_help").into_owned()
        }
    }
}

fn custom_model_placeholder(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "gpt-6.7-codex-ultra-preview",
        ProviderKind::ClaudeCode | ProviderKind::Acp => "claude-sonnet-5",
    }
}

fn path_string(path: &Option<std::path::PathBuf>) -> String {
    path.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// An input's trimmed value, or `None` when it is empty (T3: an empty field
/// removes the override).
fn trimmed(input: &Entity<InputState>, cx: &App) -> Option<String> {
    let value = input.read(cx).value().trim().to_string();
    (!value.is_empty()).then_some(value)
}
