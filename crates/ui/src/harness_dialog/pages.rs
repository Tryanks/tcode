use super::*;

impl HarnessDialog {
    pub(super) fn render_profile(
        &mut self,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let Some(description) = describe(store, &self.harness) else {
            return crate::material::empty_row(crate::tr!("composer.loading"), cx)
                .into_any_element();
        };
        let builtin = profile_id == self.builtin_id();
        let endpoint = self.is_endpoint_profile(cx);
        let own = store.provider_profile_own_settings(&profile_id);
        let home = store.provider_profile_settings(&profile_id).home_path;
        let local = !store.is_remote();
        let mut column = v_flex().w_full().gap(crate::material::section_gap(cx));
        // What is wrong with this profile alone, in the CLI's or its own words.
        let trouble = store
            .provider_profile_snapshot(&profile_id)
            .filter(|snapshot| {
                matches!(
                    snapshot.status,
                    Some(ProviderStatusKind::Warning | ProviderStatusKind::Error)
                ) && !matches!(
                    snapshot.diagnostic,
                    Some(ProviderProbeDiagnostic::Unauthenticated)
                )
            })
            .map(|snapshot| crate::provider_status::snapshot_detail(&description, &snapshot))
            .filter(|detail| !detail.is_empty());
        if let Some(trouble) = trouble {
            column = column.child(
                crate::material::notice(cx.theme().warning, cx)
                    .debug_selector(|| "harness-banner-profile".into())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .child(trouble),
                    ),
            );
        }
        column = column.children(if endpoint {
            Some(self.render_endpoint(&description, window, cx))
        } else {
            self.render_sign_in(&description, &profile_id, window, cx)
        });
        column = column.child(self.render_identity(&profile_id, window, cx));
        if !endpoint {
            column = column.child(self.render_models(&profile_id, window, cx));
        }
        // Advanced: where the account keeps itself, and what it alone sets.
        let mut advanced = Vec::new();
        if let Some(variable) = description.config_dir_env.clone() {
            if builtin {
                advanced.push(self.field_row(
                    Field::ConfigDir,
                    crate::tr!("providers.harness.config_dir").into_owned(),
                    Some(variable),
                    description.default_config_dir.clone(),
                    window,
                    cx,
                ));
            } else if let Some(home) = home {
                let shown = home.display().to_string();
                advanced.push(self.row(
                    crate::tr!("providers.harness.config_dir").into_owned(),
                    Some(shown),
                    local.then(|| {
                        h_flex()
                            .w_full()
                            .justify_end()
                            .child(
                                Button::new("harness-reveal-home")
                                    .ghost()
                                    .xsmall()
                                    .label(crate::tr!("chat.reveal_in_file_manager"))
                                    .on_click(move |_, _, cx| cx.reveal_path(&home)),
                            )
                            .into_any_element()
                    }),
                    cx,
                ));
            }
        }
        if own.as_ref().is_some_and(|own| own.binary_path.is_some()) {
            advanced.push(self.field_row(
                Field::OwnBinary,
                crate::tr!("providers.harness.binary").into_owned(),
                Some(crate::tr!("providers.harness.own_override").into_owned()),
                None,
                window,
                cx,
            ));
        }
        if own.as_ref().is_some_and(|own| own.launch_args.is_some()) {
            advanced.push(self.field_row(
                Field::OwnArgs,
                crate::tr!("providers.launch_args").into_owned(),
                Some(crate::tr!("providers.harness.own_override").into_owned()),
                None,
                window,
                cx,
            ));
        }
        let env = self.profile_env(window, cx);
        let unreadable = self.secrets_unreadable(cx);
        advanced.push(
            v_flex()
                .w_full()
                .px(px(crate::material::CARD_INSET))
                .py(px(10.))
                .gap_1()
                .child(row_labels(
                    if endpoint {
                        crate::tr!("providers.harness.other_env").into_owned()
                    } else {
                        crate::tr!("providers.env.title").into_owned()
                    },
                    Some(
                        crate::tr!("providers.harness.this_account")
                            .into_owned()
                            .into(),
                    ),
                    None,
                    self.compact(),
                    cx,
                ))
                .children(unreadable)
                .child(env)
                .into_any_element(),
        );
        column = column.child(self.group(
            Some(crate::tr!("providers.harness.advanced").into_owned()),
            advanced,
            cx,
        ));
        if !builtin {
            let id = profile_id.clone();
            let (title, body) = if endpoint {
                (
                    crate::tr!("providers.harness.delete_endpoint").into_owned(),
                    crate::tr!("providers.harness.delete_endpoint_body").into_owned(),
                )
            } else {
                (
                    crate::tr!("providers.harness.delete_account").into_owned(),
                    crate::tr!("providers.harness.delete_account_body").into_owned(),
                )
            };
            column = column.child(self.danger_row(
                "harness-delete-profile",
                title,
                body,
                crate::tr!("providers.harness.delete").into_owned(),
                cx.listener(move |this, _, window, cx| this.confirm_delete(id.clone(), window, cx)),
                cx,
            ));
        }
        column.into_any_element()
    }

    pub(super) fn render_add(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let endpoint_page = self.page == Page::AddEndpoint;
        let mut column = v_flex().w_full().gap_3();
        let presets = self.presets(cx);
        if endpoint_page {
            let mut tabs = TabBar::new("harness-presets").segmented();
            for (index, preset) in presets.iter().enumerate() {
                tabs = tabs.item(
                    TabItem::new(("harness-preset", index), preset.name.clone())
                        .selected(self.preset == Some(index))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.preset = Some(index);
                            this.seed_preset(window, cx);
                            cx.notify();
                        })),
                );
            }
            tabs = tabs.item(
                TabItem::new(
                    "harness-preset-custom",
                    crate::tr!("providers.harness.custom").into_owned(),
                )
                .selected(self.preset.is_none())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.preset = None;
                    this.seed_preset(window, cx);
                    cx.notify();
                })),
            );
            column = column.child(tabs).child(
                div().text_secondary(false, cx).child(
                    match self.preset.and_then(|index| presets.get(index)) {
                        Some(preset) => {
                            crate::tr!("providers.harness.preset_help", name = preset.name.clone())
                                .into_owned()
                        }
                        None => crate::tr!("providers.third_party.custom_help").into_owned(),
                    },
                ),
            );
        } else {
            column = column.child(
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!("providers.harness.add_account_help")),
            );
        }
        let mut rows = vec![self.field_row(
            Field::AddName,
            crate::tr!("providers.harness.name").into_owned(),
            None,
            (!endpoint_page).then(|| crate::tr!("providers.account.name_hint").into_owned()),
            window,
            cx,
        )];
        if endpoint_page {
            rows.push(self.field_row(
                Field::AddBaseUrl,
                crate::tr!("providers.third_party.base_url").into_owned(),
                None,
                None,
                window,
                cx,
            ));
            rows.push(self.field_row(
                Field::AddModel,
                crate::tr!("providers.third_party.model").into_owned(),
                None,
                None,
                window,
                cx,
            ));
            rows.push(self.field_row(
                Field::AddKey,
                crate::tr!("providers.third_party.key").into_owned(),
                None,
                Some(crate::tr!("providers.third_party.key_hint").into_owned()),
                window,
                cx,
            ));
        }
        rows.push(
            ToggleRow::new(
                "harness-inherit-config",
                crate::tr!("providers.inherit_config").into_owned(),
                self.inherit_config,
                self.compact(),
                cx.listener(|this, checked: &bool, _, cx| {
                    this.inherit_config = *checked;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        column = column.child(crate::material::grouped(rows, cx));
        if let Some(error) = self.add_error.clone() {
            column = column.child(
                div()
                    .text_secondary(false, cx)
                    .text_color(cx.theme().danger_foreground)
                    .child(error),
            );
        }
        column
            .child(
                h_flex().w_full().justify_end().gap_2().child(
                    Button::new("harness-add")
                        .debug_selector(|| "harness-add".into())
                        .primary()
                        .small()
                        .loading(self.adding)
                        .label(if endpoint_page {
                            crate::tr!("providers.harness.add")
                        } else {
                            crate::tr!("providers.login.sign_in")
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                ),
            )
            .into_any_element()
    }
}
