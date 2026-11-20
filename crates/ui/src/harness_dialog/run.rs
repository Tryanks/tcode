use super::*;

impl HarnessDialog {
    pub(super) fn render_run(
        &mut self,
        description: &HarnessDescription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shared = self.lists_accounts(cx);
        let mut rows = Vec::new();
        rows.extend(self.default_permission_row(description, cx));
        if let Some(binary) = description.binary_name.clone() {
            rows.push(
                self.field_row(
                    Field::Binary,
                    crate::tr!("providers.harness.binary").into_owned(),
                    shared.then(|| crate::tr!("providers.harness.all_accounts").into_owned()),
                    Some(
                        crate::tr!("providers.harness.binary_placeholder", binary = binary)
                            .into_owned(),
                    ),
                    window,
                    cx,
                ),
            );
        }
        if !shared && let Some(variable) = description.config_dir_env.clone() {
            rows.push(self.field_row(
                Field::ConfigDir,
                crate::tr!("providers.harness.config_dir").into_owned(),
                Some(variable),
                description.default_config_dir.clone(),
                window,
                cx,
            ));
        }
        if description.launch_args {
            rows.push(self.field_row(
                Field::LaunchArgs,
                crate::tr!("providers.launch_args").into_owned(),
                shared.then(|| crate::tr!("providers.harness.all_accounts").into_owned()),
                None,
                window,
                cx,
            ));
        }
        for toggle in description.toggles.clone() {
            let enabled = self
                .store
                .read(cx)
                .provider_profile_settings(self.builtin_id())
                .toggle(&toggle);
            let builtin = self.builtin_id().to_owned();
            let id = toggle.clone();
            let selector = format!("harness-toggle-{toggle}");
            rows.push(
                div()
                    .w_full()
                    .debug_selector(move || selector.clone())
                    .child(
                        ToggleRow::new(
                            SharedString::from(format!("harness-toggle-{toggle}")),
                            crate::tr!(&format!("providers.toggle.{toggle}")).into_owned(),
                            enabled,
                            self.compact(),
                            cx.listener(move |this, checked: &bool, _, cx| {
                                this.update_profile(
                                    builtin.clone(),
                                    ProfileSettingsPatch::SetToggle {
                                        id: id.clone(),
                                        enabled: *checked,
                                    },
                                    cx,
                                )
                            }),
                        )
                        .description(
                            crate::tr!(&format!("providers.toggle.{toggle}_help")).into_owned(),
                        ),
                    )
                    .into_any_element(),
            );
        }
        let env = self.harness_env(window, cx);
        let unreadable = self.secrets_unreadable(cx);
        rows.push(
            v_flex()
                .w_full()
                .px(px(crate::material::CARD_INSET))
                .py(px(10.))
                .gap_1()
                .child(row_labels(
                    crate::tr!("providers.env.title").into_owned(),
                    shared.then(|| {
                        crate::tr!("providers.harness.all_accounts")
                            .into_owned()
                            .into()
                    }),
                    None,
                    self.compact(),
                    cx,
                ))
                .children(unreadable)
                .child(env)
                .into_any_element(),
        );
        v_flex()
            .w_full()
            .debug_selector(|| "harness-run".into())
            .child(self.group(
                Some(crate::tr!("providers.harness.run").into_owned()),
                rows,
                cx,
            ))
            .into_any_element()
    }

    /// What a new thread may do without asking, in the harness's own terms,
    /// where it has a choice and the thread's project names none.
    pub(super) fn default_permission_row(
        &self,
        description: &HarnessDescription,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let tcode_protocol::HarnessPermission {
            id,
            options,
            recommended,
        } = description.permission.clone()?;
        let builtin = self.builtin_id().to_owned();
        let chosen = self
            .store
            .read(cx)
            .provider_profile_settings(&builtin)
            .default_permission
            .clone();
        let current = chosen.clone().or(recommended);
        let selected = options
            .iter()
            .find(|option| Some(&option.value) == current.as_ref())
            .map(|option| crate::i18n::permission_option(&id, option).0)
            .unwrap_or_default();
        let options = options
            .iter()
            .map(|option| {
                let (label, description) = crate::i18n::permission_option(&id, option);
                crate::settings_rows::SelectRowOption {
                    value: option.value.clone(),
                    id: SharedString::from(format!("harness-permission-{}", option.value)),
                    label: label.into(),
                    description: description.map(Into::into),
                    selected: Some(&option.value) == current.as_ref(),
                }
            })
            .collect();
        let reset = crate::settings_rows::reset_action(
            "harness-permission-reset",
            chosen.is_some(),
            cx.listener({
                let builtin = builtin.clone();
                move |this, _, _, cx| {
                    this.update_profile(
                        builtin.clone(),
                        ProfileSettingsPatch::SetDefaultPermission(None),
                        cx,
                    )
                }
            }),
        );
        let dialog = cx.entity();
        Some(crate::settings_rows::select_row(
            "harness-permission",
            "harness-permission-popover",
            "harness-permission-menu",
            320.,
            crate::tr!("providers.harness.default_permission")
                .into_owned()
                .into(),
            Some(
                crate::tr!("providers.harness.default_permission_help")
                    .into_owned()
                    .into(),
            ),
            selected.into(),
            options,
            reset,
            crate::settings_rows::flip_row(&self.motion, "harness-permission", self.compact()),
            move |value, _, cx| {
                dialog.update(cx, |this, cx| {
                    this.update_profile(
                        builtin.clone(),
                        ProfileSettingsPatch::SetDefaultPermission(Some(value)),
                        cx,
                    )
                })
            },
            cx,
        ))
    }

    /// The servers the harness's own configuration lists, read-only: Tcode
    /// shows them and leaves them to the harness.
    pub(super) fn render_native_mcp(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = match self.native_mcp.as_ref()? {
            Ok(servers) if servers.is_empty() => vec![self.row(
                crate::tr!("providers.harness.native_mcp_empty").into_owned(),
                None,
                None,
                cx,
            )],
            Ok(servers) => servers
                .iter()
                .map(|server| {
                    let selector = format!("harness-native-mcp-{}", server.name);
                    let target = if server.enabled {
                        server.target.clone()
                    } else {
                        crate::tr!("providers.harness.native_mcp_off", target = server.target)
                            .into_owned()
                    };
                    div()
                        .w_full()
                        .debug_selector(move || selector.clone())
                        .child(self.row(server.name.clone(), Some(target), None, cx))
                        .into_any_element()
                })
                .collect(),
            Err(error) => vec![self.row(
                crate::tr!("providers.harness.native_mcp_failed").into_owned(),
                Some(error.clone()),
                None,
                cx,
            )],
        };
        let name = harness_name(self.store.read(cx), &self.harness);
        Some(
            v_flex()
                .w_full()
                .debug_selector(|| "harness-native-mcp".into())
                .child(self.group(
                    Some(crate::tr!("providers.harness.native_mcp").into_owned()),
                    rows,
                    cx,
                ))
                .child(crate::settings_rows::footnote(
                    crate::tr!("providers.harness.native_mcp_help", name = name),
                    self.compact(),
                    cx,
                ))
                .into_any_element(),
        )
    }

    pub(super) fn render_integrations(
        &self,
        description: &HarnessDescription,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let mut rows = Vec::new();
        let name = harness_name(self.store.read(cx), &self.harness);
        if description.plugins.manages() {
            let builtin = self.builtin_id().to_owned();
            rows.push(
                crate::material::accessible_clickable(
                    crate::material::group_row(cx),
                    "harness-plugins",
                    gpui::Role::Button,
                    crate::tr!("providers.harness.plugins").into_owned(),
                    cx,
                )
                .debug_selector(|| "harness-plugins".into())
                .cursor_pointer()
                .relative()
                .child(crate::material::row_hover(px(0.), cx))
                .child(row_labels(
                    crate::tr!("providers.harness.plugins").into_owned(),
                    Some(
                        crate::tr!("providers.harness.plugins_help")
                            .into_owned()
                            .into(),
                    ),
                    None,
                    self.compact(),
                    cx,
                ))
                .child(Icon::new(IconName::ChevronRight).text_color(cx.theme().muted_foreground))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.manage_plugins(builtin.clone(), window, cx)
                }))
                .into_any_element(),
            );
        }
        if !description.mcp_servers {
            rows.push(self.row(
                crate::tr!("providers.harness.mcp").into_owned(),
                Some(crate::tr!("providers.mcp_unavailable").into_owned()),
                None,
                cx,
            ));
        }
        if description.native_computer_use {
            rows.push(self.row(
                crate::tr!("providers.harness.computer_use").into_owned(),
                Some(crate::tr!("providers.native_computer_use", name = name).into_owned()),
                None,
                cx,
            ));
        }
        (!rows.is_empty()).then(|| {
            self.group(
                Some(crate::tr!("providers.harness.integrations").into_owned()),
                rows,
                cx,
            )
        })
    }

    pub(super) fn danger_row(
        &self,
        id: &'static str,
        title: String,
        body: String,
        action: String,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> AnyElement {
        crate::material::grouped(
            vec![
                crate::material::group_row(cx)
                    .child(row_labels(
                        title,
                        Some(body.into()),
                        None,
                        self.compact(),
                        cx,
                    ))
                    .child(
                        Button::new(id)
                            .debug_selector(move || id.into())
                            .danger()
                            .xsmall()
                            .label(action)
                            .on_click(on_click),
                    )
                    .into_any_element(),
            ],
            cx,
        )
        .into_any_element()
    }
}
