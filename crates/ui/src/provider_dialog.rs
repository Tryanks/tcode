//! The per-profile provider settings dialog (Settings → Providers → gear).
//!
//! A transactional editor: it seeds a draft from the profile's current
//! [`ProviderSettings`] plus its effective launch env (for secret placeholders),
//! edits that draft in isolation, and persists only on Save. Cancel discards
//! everything, including pending secrets. Favorite toggling is the one live
//! exception — favorites are a global, not a `ProviderSettings` field.

use std::collections::HashSet;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState},
    switch::Switch,
    v_flex,
};

use agent::ProviderKind;
use tcode_runtime::app::AppState;

use crate::provider_models::{
    ResolvedModel, model_capability_label, slug_error_message, validate_slug,
};
use crate::settings::{ACCENT_PRESETS, EnvVar, provider_label};

/// One environment-variable row's live inputs and its draft flags.
struct EnvRow {
    name: Entity<InputState>,
    value: Entity<InputState>,
    sensitive: bool,
    /// A sensitive row whose value is already stored in `secrets.json`. Its
    /// value input starts empty behind the "Stored secret" placeholder.
    stored: bool,
}

/// A serializable snapshot of an env row, used to rebuild the inputs after an
/// add / remove / sensitivity toggle without losing the other rows' edits.
struct EnvSeed {
    name: String,
    value: String,
    sensitive: bool,
    stored: bool,
}

pub struct ProviderDialog {
    app_state: Entity<AppState>,
    provider: ProviderKind,
    profile_id: String,
    display_name: Entity<InputState>,
    binary: Entity<InputState>,
    home: Entity<InputState>,
    launch_args: Entity<InputState>,
    custom_model: Entity<InputState>,
    slug_error: Option<String>,
    /// Draft accent (`#rrggbb`); applied on Save.
    accent: Option<String>,
    env_rows: Vec<EnvRow>,
    /// Names that held a persisted secret when the dialog opened. On Save, any
    /// that is no longer a sensitive row is cleared from `secrets.json`.
    original_secret_names: Vec<String>,
    custom_models: Vec<String>,
    hidden_models: Vec<String>,
    _subscriptions: Vec<Subscription>,
}

impl ProviderDialog {
    pub fn new(
        app_state: Entity<AppState>,
        provider: ProviderKind,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = app_state.read(cx).profile_settings(&profile_id);
        // Which of this profile's names resolve to a value at launch: a sensitive
        // row present here has its secret saved (see `launch_env_for_profile`).
        let stored: HashSet<String> = app_state
            .read(cx)
            .launch_env_for_profile(&profile_id)
            .env
            .iter()
            .map(|(name, _)| name.clone())
            .collect();

        let text_input =
            |placeholder: String, value: String, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    let mut input = InputState::new(window, cx).placeholder(placeholder);
                    input.set_value(value, window, cx);
                    input
                })
            };

        let display_name = text_input(
            provider_label(provider).to_string(),
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
        let launch_args = text_input(
            launch_args_placeholder(provider).to_string(),
            settings.launch_args.clone().unwrap_or_default(),
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
        subscriptions.push(
            cx.subscribe_in(&custom_model, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_custom_model(window, cx);
                }
            }),
        );

        let original_secret_names: Vec<String> = settings
            .env
            .iter()
            .filter(|var| var.sensitive && stored.contains(&var.name))
            .map(|var| var.name.clone())
            .collect();

        let seeds: Vec<EnvSeed> = settings
            .env
            .iter()
            .map(|var| EnvSeed {
                name: var.name.clone(),
                value: if var.sensitive {
                    String::new()
                } else {
                    var.value.clone()
                },
                sensitive: var.sensitive,
                stored: var.sensitive && stored.contains(&var.name),
            })
            .collect();

        let mut dialog = Self {
            app_state,
            provider,
            profile_id,
            display_name,
            binary,
            home,
            launch_args,
            custom_model,
            slug_error: None,
            accent: settings.accent_color.clone(),
            env_rows: Vec::new(),
            original_secret_names,
            custom_models: settings.custom_models.clone(),
            hidden_models: settings.hidden_models.clone(),
            _subscriptions: subscriptions,
        };
        dialog.rebuild_env_rows(&seeds, window, cx);
        dialog
    }

    /// Whether this dialog edits a user-created profile (offer Delete) rather
    /// than a built-in one.
    fn is_user_profile(&self) -> bool {
        !tcode_core::settings::Settings::is_builtin_profile_id(&self.profile_id)
    }

    // -- persistence (Save) -------------------------------------------------

    /// Persist the whole draft: card settings, then secret writes/clears, then a
    /// single provider reload. `enabled` is owned by the row switch and left as
    /// it is. Called only from Save.
    fn apply(&mut self, cx: &mut Context<Self>) {
        let seeds = self.snapshot_env(cx);
        let mut env: Vec<EnvVar> = Vec::new();
        let mut secret_writes: Vec<(String, String)> = Vec::new();
        let mut final_sensitive: HashSet<String> = HashSet::new();
        for seed in &seeds {
            let name = seed.name.trim().to_string();
            if name.is_empty() {
                // A nameless variable can't be passed to a child; drop it.
                continue;
            }
            if seed.sensitive {
                // An empty input means "keep the stored secret as it is".
                if !seed.value.is_empty() {
                    secret_writes.push((name.clone(), seed.value.clone()));
                }
                env.push(EnvVar {
                    name: name.clone(),
                    value: String::new(),
                    sensitive: true,
                });
                final_sensitive.insert(name);
            } else {
                env.push(EnvVar {
                    name,
                    value: seed.value.clone(),
                    sensitive: false,
                });
            }
        }
        // Secrets whose row was removed, renamed, or turned plaintext are cleared.
        let clears: Vec<String> = self
            .original_secret_names
            .iter()
            .filter(|name| !final_sensitive.contains(*name))
            .cloned()
            .collect();

        let display_name = trimmed(&self.display_name, cx);
        let binary = trimmed(&self.binary, cx);
        let home = trimmed(&self.home, cx);
        let launch = trimmed(&self.launch_args, cx);
        let accent = self.accent.clone();
        let custom = self.custom_models.clone();
        let hidden = self.hidden_models.clone();
        let provider = self.provider;
        let profile_id = self.profile_id.clone();

        self.app_state.update(cx, |state, cx| {
            state.update_profile_settings(
                &profile_id,
                move |settings| {
                    settings.display_name = display_name;
                    settings.accent_color = accent;
                    settings.env = env;
                    settings.binary_path = binary.map(Into::into);
                    // OpenCode has no single-home override.
                    settings.home_path = (provider != ProviderKind::OpenCode)
                        .then(|| home.map(Into::into))
                        .flatten();
                    // Codex ignores launch arguments (no field is rendered).
                    settings.launch_args = match provider {
                        ProviderKind::Codex => None,
                        _ => launch,
                    };
                    settings.custom_models = custom;
                    settings.hidden_models = hidden;
                },
                cx,
            );
            for (name, value) in &secret_writes {
                state.set_profile_secret(&profile_id, name, Some(value), cx);
            }
            for name in &clears {
                state.set_profile_secret(&profile_id, name, None, cx);
            }
            state.reload_provider(provider, cx);
        });
    }

    // -- environment variables ---------------------------------------------

    /// Read the live env inputs back into seeds (preserves other rows' edits
    /// across a rebuild).
    fn snapshot_env(&self, cx: &App) -> Vec<EnvSeed> {
        self.env_rows
            .iter()
            .map(|row| EnvSeed {
                name: row.name.read(cx).value().to_string(),
                value: row.value.read(cx).value().to_string(),
                sensitive: row.sensitive,
                stored: row.stored,
            })
            .collect()
    }

    fn rebuild_env_rows(&mut self, seeds: &[EnvSeed], window: &mut Window, cx: &mut Context<Self>) {
        self.env_rows.clear();
        for seed in seeds {
            let name = cx.new(|cx| {
                let mut input = InputState::new(window, cx)
                    .placeholder(tcode_i18n::tr!("providers.env.name_placeholder"));
                input.set_value(seed.name.clone(), window, cx);
                input
            });
            let show_stored = seed.sensitive && seed.stored && seed.value.is_empty();
            let value = cx.new(|cx| {
                let placeholder = if show_stored {
                    tcode_i18n::tr!("providers.env.stored_secret")
                } else {
                    tcode_i18n::tr!("providers.env.value_placeholder")
                };
                let mut input = InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(seed.sensitive);
                input.set_value(seed.value.clone(), window, cx);
                input
            });
            self.env_rows.push(EnvRow {
                name,
                value,
                sensitive: seed.sensitive,
                stored: seed.stored,
            });
        }
    }

    fn add_env_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut seeds = self.snapshot_env(cx);
        // New variables default to sensitive.
        seeds.push(EnvSeed {
            name: String::new(),
            value: String::new(),
            sensitive: true,
            stored: false,
        });
        self.rebuild_env_rows(&seeds, window, cx);
        cx.notify();
    }

    fn remove_env_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut seeds = self.snapshot_env(cx);
        if index >= seeds.len() {
            return;
        }
        seeds.remove(index);
        self.rebuild_env_rows(&seeds, window, cx);
        cx.notify();
    }

    fn toggle_env_sensitive(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut seeds = self.snapshot_env(cx);
        let Some(seed) = seeds.get_mut(index) else {
            return;
        };
        seed.sensitive = !seed.sensitive;
        // Its state changed, so it is no longer the untouched stored secret; the
        // masked value (plaintext ↔ secret) rides along in the draft until Save.
        seed.stored = false;
        self.rebuild_env_rows(&seeds, window, cx);
        cx.notify();
    }

    // -- models -------------------------------------------------------------

    fn add_custom_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.custom_model.read(cx).value().to_string();
        let catalog = self.app_state.read(cx).profile_catalog(&self.profile_id);
        let mut draft = self.app_state.read(cx).profile_settings(&self.profile_id);
        draft.custom_models = self.custom_models.clone();
        match validate_slug(&raw, &catalog, &draft) {
            Ok(slug) => {
                self.slug_error = None;
                self.custom_models.push(slug);
                self.custom_model
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
            Err(err) => self.slug_error = Some(slug_error_message(&err)),
        }
        cx.notify();
    }

    fn toggle_hidden(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(pos) = self.hidden_models.iter().position(|m| m == id) {
            self.hidden_models.remove(pos);
        } else {
            self.hidden_models.push(id.to_string());
        }
        cx.notify();
    }

    fn remove_custom_model(&mut self, id: &str, cx: &mut Context<Self>) {
        self.custom_models.retain(|m| m != id);
        self.hidden_models.retain(|m| m != id);
        cx.notify();
    }

    // -- rendering ----------------------------------------------------------

    /// A labelled section: an 11px caption over its field blocks.
    fn section(
        &self,
        label: SharedString,
        children: Vec<AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        v_flex()
            .w_full()
            .gap_3()
            .child(
                div()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .children(children)
            .into_any_element()
    }

    /// A labelled field block: label, control, optional muted help text.
    fn field_block(
        &self,
        label: SharedString,
        help: SharedString,
        control: AnyElement,
        cx: &Context<Self>,
    ) -> AnyElement {
        v_flex()
            .w_full()
            .gap_1p5()
            .child(div().text_size(px(13.)).font_medium().child(label))
            .child(control)
            .when(!help.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .child(help),
                )
            })
            .into_any_element()
    }

    fn render_identity(&self, cx: &mut Context<Self>) -> AnyElement {
        self.section(
            tcode_i18n::tr!("providers.dialog.identity")
                .into_owned()
                .into(),
            vec![
                self.field_block(
                    tcode_i18n::tr!("providers.display_name")
                        .into_owned()
                        .into(),
                    tcode_i18n::tr!("providers.display_name_help")
                        .into_owned()
                        .into(),
                    Input::new(&self.display_name)
                        .rounded(crate::material::radius_input())
                        .into_any_element(),
                    cx,
                ),
                self.field_block(
                    tcode_i18n::tr!("providers.accent").into_owned().into(),
                    tcode_i18n::tr!("providers.accent_help").into_owned().into(),
                    self.render_accent(cx),
                    cx,
                ),
            ],
            cx,
        )
    }

    fn render_accent(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.accent.clone();
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
                                tcode_i18n::tr!("providers.accent_select", color = hex.clone())
                                    .into_owned();
                            gpui_component::tooltip::Tooltip::new(label).build(window, cx)
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.accent = Some(hex.clone());
                        cx.notify();
                    })),
            );
        }
        swatches = swatches.child(
            Button::new("accent-clear")
                .ghost()
                .xsmall()
                .icon(IconName::CircleX)
                .tooltip(tcode_i18n::tr!("providers.accent_clear"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.accent = None;
                    cx.notify();
                })),
        );
        swatches.into_any_element()
    }

    fn render_connection(&self, cx: &mut Context<Self>) -> AnyElement {
        let provider = self.provider;
        let mut blocks = vec![
            self.field_block(
                tcode_i18n::tr!("providers.binary_path").into_owned().into(),
                tcode_i18n::tr!(
                    "providers.binary_path_help",
                    name = provider_label(provider)
                )
                .into_owned()
                .into(),
                Input::new(&self.binary)
                    .rounded(crate::material::radius_input())
                    .into_any_element(),
                cx,
            ),
        ];
        if provider != ProviderKind::OpenCode {
            blocks.push(
                self.field_block(
                    home_label(provider).into(),
                    home_help(provider).into(),
                    Input::new(&self.home)
                        .rounded(crate::material::radius_input())
                        .into_any_element(),
                    cx,
                ),
            );
        }
        blocks.push(self.render_env(cx));
        if provider != ProviderKind::Codex {
            blocks.push(
                self.field_block(
                    tcode_i18n::tr!("providers.launch_args").into_owned().into(),
                    tcode_i18n::tr!("providers.launch_args_help")
                        .into_owned()
                        .into(),
                    Input::new(&self.launch_args)
                        .rounded(crate::material::radius_input())
                        .into_any_element(),
                    cx,
                ),
            );
        }
        self.section(
            tcode_i18n::tr!("providers.dialog.connection")
                .into_owned()
                .into(),
            blocks,
            cx,
        )
    }

    fn render_env(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let mut block = v_flex().w_full().gap_2().child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(13.))
                        .font_medium()
                        .child(tcode_i18n::tr!("providers.env.title")),
                )
                .child(
                    Button::new("env-add")
                        .outline()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label(tcode_i18n::tr!("providers.env.add"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.add_env_row(window, cx);
                        })),
                ),
        );

        if self.env_rows.is_empty() {
            block = block.child(
                div()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(tcode_i18n::tr!("providers.env.empty_help")),
            );
        } else {
            for (index, row) in self.env_rows.iter().enumerate() {
                block =
                    block.child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .child(div().flex_1().min_w_0().child(
                                Input::new(&row.name).rounded(crate::material::radius_input()),
                            ))
                            .child(div().flex_1().min_w_0().child(
                                Input::new(&row.value).rounded(crate::material::radius_input()),
                            ))
                            .child(
                                Switch::new(("env-sensitive", index))
                                    .checked(row.sensitive)
                                    .tooltip(tcode_i18n::tr!("providers.env.sensitive"))
                                    .on_click(cx.listener(move |this, _: &bool, window, cx| {
                                        this.toggle_env_sensitive(index, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(("env-remove", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Delete)
                                    .tooltip(tcode_i18n::tr!("providers.env.remove"))
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
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(tcode_i18n::tr!("providers.env.security_help")),
            )
            .into_any_element()
    }

    fn render_models(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.app_state.read(cx).draft_models_for_profile(
            &self.profile_id,
            &self.custom_models,
            &self.hidden_models,
        );
        let muted = cx.theme().muted_foreground;

        let mut block = v_flex().w_full().gap_1().child(
            div()
                .pb_1()
                .text_size(px(13.))
                .text_color(muted)
                .child(if rows.len() == 1 {
                    tcode_i18n::tr!("providers.models.count_one", count = 1).into_owned()
                } else {
                    tcode_i18n::tr!("providers.models.count", count = rows.len()).into_owned()
                }),
        );

        for (index, row) in rows.iter().enumerate() {
            block = block.child(self.render_model_row(index, row, cx));
        }

        // Custom-model input + validation copy.
        block = block.child(
            h_flex()
                .w_full()
                .pt_2()
                .gap_2()
                .items_center()
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&self.custom_model).rounded(crate::material::radius_input()),
                    ),
                )
                .child(
                    Button::new("add-custom-model")
                        .outline()
                        .small()
                        .icon(IconName::Plus)
                        .label(tcode_i18n::tr!("providers.models.add"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_custom_model(window, cx)),
                        ),
                ),
        );
        if let Some(error) = &self.slug_error {
            block = block.child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().danger_foreground)
                    .child(error.clone()),
            );
        }
        self.section(
            tcode_i18n::tr!("providers.models.title")
                .into_owned()
                .into(),
            vec![block.into_any_element()],
            cx,
        )
    }

    fn render_model_row(
        &self,
        index: usize,
        row: &ResolvedModel,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let name = row.name.clone();
        let hidden = row.hidden;
        let capabilities = row
            .capabilities
            .iter()
            .copied()
            .map(model_capability_label)
            .collect::<Vec<_>>()
            .join(" · ");

        let mut tags = h_flex().gap_1().items_center();
        if row.custom {
            tags = tags.child(tag(
                tcode_i18n::tr!("providers.models.custom").into_owned(),
                cx.theme().info.opacity(0.12),
                cx.theme().info_foreground,
            ));
        }
        if hidden {
            tags = tags.child(tag(
                tcode_i18n::tr!("providers.models.hidden").into_owned(),
                cx.theme().warning.opacity(0.12),
                cx.theme().warning_foreground,
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
                // Favorites are a global, not part of the draft — this writes live.
                Button::new(("model-fav", index))
                    .ghost()
                    .xsmall()
                    .icon(if is_fav {
                        IconName::StarFill
                    } else {
                        IconName::Star
                    })
                    .tooltip(if is_fav {
                        tcode_i18n::tr!("providers.models.unfavorite")
                    } else {
                        tcode_i18n::tr!("providers.models.favorite")
                    })
                    .on_click(move |_, _, cx| {
                        app_state.update(cx, |state, cx| state.toggle_favorite_model(&fav_id, cx));
                    }),
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
                        tcode_i18n::tr!("providers.models.show")
                    } else {
                        tcode_i18n::tr!("providers.models.hide")
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_hidden(&hide_id, cx))),
            )
            .when(custom, |this| {
                this.child(
                    Button::new(("model-remove", index))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Delete)
                        .tooltip(tcode_i18n::tr!("providers.models.remove"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_custom_model(&remove_id, cx);
                        })),
                )
            })
            .into_any_element()
    }
}

impl Render for ProviderDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The dialog's content_builder path has no built-in scroll, so cap and
        // scroll the form body ourselves.
        div()
            .id("provider-dialog-body")
            .w_full()
            .max_h(px(520.))
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .gap(px(20.))
                    .child(self.render_identity(cx))
                    .child(self.render_connection(cx))
                    .child(self.render_models(cx)),
            )
    }
}

/// The dialog footer: Delete (user profiles only, left) + Cancel / Save (right).
pub fn render_footer(
    dialog: &Entity<ProviderDialog>,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let (is_user, profile_id, app_state) = {
        let d = dialog.read(cx);
        (
            d.is_user_profile(),
            d.profile_id.clone(),
            d.app_state.clone(),
        )
    };
    let save_dialog = dialog.clone();

    let mut left = div().flex_1();
    if is_user {
        let delete_id = profile_id.clone();
        let delete_state = app_state.clone();
        left = left.child(
            Button::new("delete-profile")
                .danger()
                .icon(IconName::Delete)
                .label(tcode_i18n::tr!("providers.delete_profile").into_owned())
                .on_click(move |_, window, cx| {
                    let delete_id = delete_id.clone();
                    let delete_state = delete_state.clone();
                    window.open_alert_dialog(cx, move |alert, _, _| {
                        let delete_id = delete_id.clone();
                        let delete_state = delete_state.clone();
                        alert
                            .title(tcode_i18n::tr!("providers.delete_confirm_title"))
                            .description(tcode_i18n::tr!("providers.delete_confirm_body"))
                            .button_props(
                                DialogButtonProps::default()
                                    .ok_variant(ButtonVariant::Danger)
                                    .ok_text(tcode_i18n::tr!("providers.delete_profile"))
                                    .cancel_text(tcode_i18n::tr!("settings.cancel"))
                                    .show_cancel(true),
                            )
                            .on_ok(move |_, window, cx| {
                                delete_state
                                    .update(cx, |state, cx| state.delete_profile(&delete_id, cx));
                                // Close the confirm and the underlying settings dialog.
                                window.close_all_dialogs(cx);
                                true
                            })
                    });
                }),
        );
    }

    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(left)
        .child(
            Button::new("provider-cancel")
                .ghost()
                .label(tcode_i18n::tr!("settings.cancel").into_owned())
                .on_click(|_, window, cx| window.close_dialog(cx)),
        )
        .child(
            Button::new("provider-save")
                .primary()
                .label(tcode_i18n::tr!("settings.save").into_owned())
                .on_click(move |_, window, cx| {
                    save_dialog.update(cx, |d, cx| d.apply(cx));
                    window.close_dialog(cx);
                }),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Copy + helpers
// ---------------------------------------------------------------------------

fn tag(label: String, background: gpui::Hsla, foreground: gpui::Hsla) -> AnyElement {
    crate::material::semantic_chip(label, background, foreground).into_any_element()
}

fn default_binary_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "codex",
        ProviderKind::ClaudeCode => "claude",
        ProviderKind::Pi => "pi",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Acp => "",
    }
}

fn home_placeholder(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "~/.codex",
        ProviderKind::ClaudeCode => "~",
        ProviderKind::Pi => "~/.pi/agent",
        ProviderKind::OpenCode => "",
        ProviderKind::Acp => "",
    }
}

fn home_label(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => tcode_i18n::tr!("providers.codex_home").into_owned(),
        ProviderKind::ClaudeCode => tcode_i18n::tr!("providers.claude_home").into_owned(),
        ProviderKind::Pi => tcode_i18n::tr!("providers.pi_home").into_owned(),
        ProviderKind::OpenCode | ProviderKind::Acp => {
            tcode_i18n::tr!("providers.home").into_owned()
        }
    }
}

fn home_help(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => tcode_i18n::tr!("providers.codex_home_help").into_owned(),
        ProviderKind::ClaudeCode => tcode_i18n::tr!("providers.claude_home_help").into_owned(),
        ProviderKind::Pi => tcode_i18n::tr!("providers.pi_home_help").into_owned(),
        ProviderKind::OpenCode | ProviderKind::Acp => {
            tcode_i18n::tr!("providers.home_help").into_owned()
        }
    }
}

fn launch_args_placeholder(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::ClaudeCode => "e.g. --chrome",
        ProviderKind::Pi => "e.g. --provider openai-codex",
        ProviderKind::OpenCode => "e.g. --print-logs",
        ProviderKind::Codex | ProviderKind::Acp => "",
    }
}

fn custom_model_placeholder(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "gpt-6.7-codex-ultra-preview",
        ProviderKind::ClaudeCode => "claude-sonnet-5",
        ProviderKind::Pi => "openai-codex/gpt-5.5",
        ProviderKind::OpenCode => "openai/gpt-5.1-codex",
        ProviderKind::Acp => "",
    }
}

fn path_string(path: &Option<std::path::PathBuf>) -> String {
    path.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// An input's trimmed value, or `None` when it is empty (an empty field removes
/// the override).
fn trimmed(input: &Entity<InputState>, cx: &App) -> Option<String> {
    let value = input.read(cx).value().trim().to_string();
    (!value.is_empty()).then_some(value)
}
