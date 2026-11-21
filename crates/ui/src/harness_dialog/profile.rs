use super::*;

impl HarnessDialog {
    pub(super) fn render_endpoint(
        &mut self,
        description: &HarnessDescription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(endpoint) = description.endpoint.clone() else {
            return div().into_any_element();
        };
        let profile = self.shown_profile();
        let stored = self
            .stored_secrets(&profile, cx)
            .contains(&endpoint.key_env);
        let mut rows = vec![self.field_row(
            Field::BaseUrl,
            crate::tr!("providers.third_party.base_url").into_owned(),
            Some(endpoint.base_url_env.clone()),
            None,
            window,
            cx,
        )];
        rows.push(self.field_row(
            Field::ApiKey,
            crate::tr!("providers.third_party.key").into_owned(),
            Some(endpoint.key_env.clone()),
            Some(if stored {
                crate::tr!("providers.harness.key_saved").into_owned()
            } else {
                crate::tr!("providers.third_party.key_hint").into_owned()
            }),
            window,
            cx,
        ));
        if let Some(model_env) = endpoint.model_env.clone() {
            rows.push(self.field_row(
                Field::Model,
                crate::tr!("providers.third_party.model").into_owned(),
                Some(model_env),
                None,
                window,
                cx,
            ));
        }
        self.group(
            Some(crate::tr!("providers.harness.endpoint").into_owned()),
            rows,
            cx,
        )
    }

    pub(super) fn render_identity(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown_as = self
            .store
            .read(cx)
            .provider_profile_display_name(profile_id);
        let name = self.field_row(
            Field::Name,
            crate::tr!("providers.harness.name").into_owned(),
            Some(crate::tr!("providers.harness.shown_as", name = shown_as).into_owned()),
            None,
            window,
            cx,
        );
        let accent = self.render_accent(profile_id, window, cx);
        let color = self.row(
            crate::tr!("providers.accent").into_owned(),
            None,
            Some(accent),
            cx,
        );
        self.group(
            Some(crate::tr!("providers.harness.name").into_owned()),
            vec![name, color],
            cx,
        )
    }

    pub(super) fn render_accent(
        &self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self
            .store
            .read(cx)
            .provider_profile_settings(profile_id)
            .accent_color;
        let mut swatches = h_flex().gap_2().items_center().flex_wrap();
        for (index, preset) in ACCENT_PRESETS.iter().enumerate() {
            let hex = (*preset).to_string();
            let selected = current
                .as_deref()
                .is_some_and(|current| current.eq_ignore_ascii_case(preset));
            let value = u32::from_str_radix(preset.trim_start_matches('#'), 16).unwrap_or(0);
            let label: SharedString = crate::tr!("providers.accent_select", color = hex.clone())
                .into_owned()
                .into();
            let profile = profile_id.to_owned();
            // The ring is always drawn, clear while unselected, so it can glide.
            let border = transition(
                ElementId::from(("accent-ring", index)),
                if selected {
                    cx.theme().foreground
                } else {
                    cx.theme().foreground.opacity(0.)
                },
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            );
            // gpui-base's `ColorSwatch` keeps its focus handle to itself (no
            // `track_focus`), leaving only `focus_visible`, which switches at
            // once, to show the keyboard's focus. So the swatch is drawn as
            // one is — a radio button named for its colour, stating both
            // toggled and selected — on a clickable that holds its handle and
            // fades its ring (`motion::focus_ring`). Back to `ColorSwatch`
            // once it takes a focus handle.
            swatches = swatches.child(
                crate::material::accessible_clickable(
                    div(),
                    ("accent", index),
                    gpui::Role::RadioButton,
                    label.clone(),
                    cx,
                )
                .aria_toggled(if selected {
                    gpui::Toggled::True
                } else {
                    gpui::Toggled::False
                })
                .aria_selected(selected)
                // A swatch is its colour at a fixed size; there is no token for it.
                .size(px(22.))
                .rounded_full()
                .cursor_pointer()
                .bg(rgb(value))
                .border_2()
                .border_color(border)
                .with_tooltip(move |_, _| crate::widgets::tooltip::Tooltip::new(label.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let color = (!selected).then(|| hex.clone());
                    this.update_profile(
                        profile.clone(),
                        ProfileSettingsPatch::SetAccentColor(color),
                        cx,
                    );
                })),
            );
        }
        swatches.into_any_element()
    }

    pub(super) fn render_models(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = self.store.read(cx).provider_profile_settings(profile_id);
        let models = self.store.read(cx).provider_dialog_models(
            profile_id,
            &settings.custom_models,
            &settings.hidden_models,
        );
        let total = models.len();
        let folded = !self.models_open && total > MODELS_SHOWN + 1;
        let mut rows: Vec<AnyElement> = models
            .iter()
            .enumerate()
            .take(if folded { MODELS_SHOWN } else { total })
            .map(|(index, model)| {
                self.model_row(profile_id, index, model, &settings.hidden_models, cx)
            })
            .collect();
        if folded {
            rows.push(
                Button::new("harness-models-more")
                    .ghost()
                    .small()
                    .w_full()
                    .label(
                        crate::tr!(
                            "providers.harness.more_models",
                            count = total - MODELS_SHOWN
                        )
                        .into_owned(),
                    )
                    .icon(IconName::ChevronDown)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.models_open = true;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let input = self.input(Field::CustomModel, window, cx);
        rows.push(
            crate::material::group_row(cx)
                .child(div().flex_1().min_w_0().child(
                    Input::new(&input).placeholder(crate::tr!("providers.harness.model_id")),
                ))
                .child(
                    Button::new("harness-add-model")
                        .outline()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label(crate::tr!("providers.models.add"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_custom_model(window, cx)),
                        ),
                )
                .into_any_element(),
        );
        let mut group = v_flex()
            .w_full()
            .debug_selector(|| "harness-models".into())
            .child(self.group(
                Some(crate::tr!("providers.models.title").into_owned()),
                rows,
                cx,
            ));
        if let Some(error) =
            crate::motion::shown("slug-error", self.slug_error.clone(), SHOW, window, cx)
        {
            group = group.child(
                error.fold(
                    div()
                        .pt_1()
                        .pl(px(crate::material::CARD_INSET))
                        .text_secondary(false, cx)
                        .text_color(cx.theme().danger_foreground)
                        .child(error.value.clone()),
                ),
            );
        }
        group.into_any_element()
    }

    pub(super) fn model_row(
        &self,
        profile_id: &str,
        index: usize,
        model: &ResolvedModel,
        hidden: &[String],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let capabilities = model
            .capabilities
            .iter()
            .copied()
            .map(model_capability_label)
            .collect::<Vec<_>>()
            .join(" · ");
        let store = self.store.clone();
        let kind = self.kind();
        let fav_profile = profile_id.to_owned();
        let fav_id = model.id.clone();
        let hide_profile = profile_id.to_owned();
        let hide_id = model.id.clone();
        let hidden = hidden.to_vec();
        let remove_profile = profile_id.to_owned();
        let remove_id = model.id.clone();
        crate::material::group_row(cx)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .items_center()
                    .text_body(false, cx)
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .when(model.hidden, |name| name.text_color(muted))
                            .child(crate::widgets::fade_text(model.name.clone())),
                    )
                    .when(!capabilities.is_empty(), |row| {
                        row.child(
                            div()
                                .id(("model-info", index))
                                .flex_none()
                                .child(Icon::new(IconName::Info).text_color(muted))
                                .with_tooltip(move |_, _| {
                                    crate::widgets::tooltip::Tooltip::new(capabilities.clone())
                                }),
                        )
                    })
                    .when(model.custom, |row| {
                        row.child(crate::material::chip(
                            crate::tr!("providers.models.custom").into_owned(),
                            cx,
                        ))
                    }),
            )
            .when(model.custom, |row| {
                row.child(
                    Button::new(("model-remove", index))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Delete)
                        .tooltip(crate::tr!("providers.models.remove"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let settings = this
                                .store
                                .read(cx)
                                .provider_profile_settings(&remove_profile);
                            let mut custom = settings.custom_models;
                            custom.retain(|id| *id != remove_id);
                            let mut hidden = settings.hidden_models;
                            hidden.retain(|id| *id != remove_id);
                            this.update_profile(
                                remove_profile.clone(),
                                ProfileSettingsPatch::SetCustomModels(custom),
                                cx,
                            );
                            this.update_profile(
                                remove_profile.clone(),
                                ProfileSettingsPatch::SetHiddenModels(hidden),
                                cx,
                            );
                        })),
                )
            })
            .child(
                Button::new(("model-fav", index))
                    .ghost()
                    .xsmall()
                    .icon(if model.favorite {
                        IconName::StarFill
                    } else {
                        IconName::Star
                    })
                    .tooltip(if model.favorite {
                        crate::tr!("providers.models.unfavorite")
                    } else {
                        crate::tr!("providers.models.favorite")
                    })
                    .on_click(move |_, _, cx| {
                        store.update(cx, |store, cx| {
                            store.toggle_favorite_model(
                                kind,
                                Some(fav_profile.clone()),
                                None,
                                fav_id.clone(),
                                cx,
                            )
                        });
                    }),
            )
            .child(
                Switch::new(("model-shown", index))
                    .checked(!model.hidden)
                    .tooltip(crate::tr!("providers.harness.show_in_picker"))
                    .on_click(cx.listener(move |this, shown: &bool, _, cx| {
                        let mut hidden = hidden.clone();
                        hidden.retain(|id| *id != hide_id);
                        if !*shown {
                            hidden.push(hide_id.clone());
                        }
                        this.update_profile(
                            hide_profile.clone(),
                            ProfileSettingsPatch::SetHiddenModels(hidden),
                            cx,
                        );
                    })),
            )
            .into_any_element()
    }
}
