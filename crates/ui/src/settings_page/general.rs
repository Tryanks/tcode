use super::*;

impl SettingsPage {
    pub(super) fn render_general(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let language = DevicePreferences::language(cx);
        let theme = DevicePreferences::theme(cx);
        let device_name_overridden = DevicePreferences::device_name_overridden(cx);
        let attachment_limit_overridden =
            DevicePreferences::remote_attachment_limit_mib(cx).is_some();
        let mut appearance = vec![
            self.language_row(language.as_deref(), cx),
            self.theme_row(theme, cx),
        ];
        if self.settings_hosts(cx).len() <= 1 {
            appearance.push(self.provider_marks_row(cx));
        }
        let device = vec![
            self.device_name_row(device_name_overridden, cx),
            self.remote_attachment_limit_row(attachment_limit_overridden, cx),
        ];
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.appearance_section"),
                appearance,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.device_section"),
                device,
                cx,
            ))
    }

    /// Whether the sidebar marks this host's threads with their provider: a
    /// setting of the host, so over several hosts it is among the threads'.
    pub(super) fn provider_marks_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let marks = self.store.read(cx).settings().sidebar_provider_marks;
        let reset = crate::settings_rows::reset_action(
            "reset-sidebar-provider-marks",
            marks,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_sidebar_provider_marks(false), cx)
            }),
        );
        self.toggle_row(
            "sidebar-provider-marks",
            crate::tr!("settings.sidebar_provider_marks.title"),
            crate::tr!("settings.sidebar_provider_marks.description"),
            marks,
            reset,
            cx,
            WorkspaceStore::set_sidebar_provider_marks,
        )
    }

    pub(super) fn render_maintenance(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let throttle_disabled = self
            .store
            .read(cx)
            .settings()
            .inactive_frame_throttle_disabled;
        let frame_throttle_reset = crate::settings_rows::reset_action(
            "reset-inactive-frame-throttle",
            throttle_disabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_inactive_frame_throttle_disabled(false),
                    cx,
                )
            }),
        );
        let performance = vec![self.toggle_row(
            "inactive-frame-throttle",
            crate::tr!("settings.inactive_frame_throttle.title"),
            crate::tr!("settings.inactive_frame_throttle.description"),
            // Stored inverted: checked = enabled.
            !throttle_disabled,
            frame_throttle_reset,
            cx,
            |store, checked| store.set_inactive_frame_throttle_disabled(!checked),
        )];
        let migration = self.store.read(cx).storage_migration().cloned();
        let migration = crate::motion::shown("storage-migration", migration, SHOW, window, cx)
            .map(|shown| shown.fold(self.storage_migration_group(&shown.value, cx)));
        v_flex()
            .gap(crate::material::section_gap(cx))
            .children(migration)
            .children(self.disk_usage_groups(window, cx))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.performance_section"),
                performance,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.reset_section"),
                vec![self.restore_all_row(cx)],
                cx,
            ))
    }

    /// The host moving an older build's threads into its current storage.
    pub(super) fn storage_migration_group(
        &self,
        migration: &tcode_protocol::StorageMigration,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let compact = self.window_state.read(cx).compact;
        let percent = 100. * migration.moved as f32 / migration.total.max(1) as f32;
        let row = crate::material::group_row_stacked(cx)
            .debug_selector(|| "storage-migration".into())
            .child(
                gpui_base::h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_caption(cx)
                            .child(crate::widgets::fade_text(crate::tr!(
                                "settings.storage_migration.title"
                            ))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_meta_mono(compact, cx)
                            .child(format!("{}%", percent.floor() as u32)),
                    ),
            )
            .child(
                crate::widgets::progress::Progress::new("storage-migration-progress")
                    .small()
                    .value(percent),
            )
            .when(migration.waiting_for_space, |row| {
                row.child(
                    div()
                        .text_secondary(compact, cx)
                        .child(crate::tr!("settings.storage_migration.waiting_for_space")),
                )
            });
        crate::settings_rows::settings_group(
            crate::tr!("settings.storage_migration.section"),
            vec![row.into_any_element()],
            cx,
        )
    }

    /// The whole-settings escape hatch: the per-item buttons cover single
    /// settings, this one covers the rest and keeps its confirm dialog.
    pub(super) fn restore_all_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let row = self.flip_row("restore-all", cx);
        let layout = row.row(
            crate::settings_rows::row_labels(
                crate::tr!("settings.reset_all"),
                Some(crate::tr!("settings.restore_description").into()),
                None,
                row.compact(),
                cx,
            ),
            Button::new("restore-defaults")
                .danger()
                .outline()
                .small()
                .label(crate::tr!("settings.restore"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.confirm_restore(window, cx);
                })),
            cx,
        );
        row.render(layout)
    }

    pub(super) fn title_generation_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let chosen = self.store.read(cx).settings().title_generation.is_some();
        let reset = crate::settings_rows::reset_action(
            "reset-title-generation",
            chosen,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_title_generation(None), cx);
            }),
        );
        let row = self.flip_row("title-generation", cx);
        let layout = row.row(
            crate::settings_rows::row_labels(
                crate::tr!("settings.title_generation.title"),
                Some(crate::tr!("settings.title_generation.description").into()),
                reset,
                row.compact(),
                cx,
            ),
            self.title_model_picker.clone(),
            cx,
        );
        row.render(layout)
    }

    pub(super) fn device_name_row(&self, overridden: bool, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let reset = crate::settings_rows::reset_action(
            "reset-device-name",
            overridden,
            cx.listener(|this, _, window, cx| {
                DevicePreferences::set_device_name(None, cx);
                let name = DevicePreferences::device_name(cx);
                this.device_name_input.push(name, window, cx);
            }),
        );
        let row = self.flip_row("device-name", cx);
        let layout = row
            .row(
                crate::settings_rows::row_labels(
                    crate::tr!("settings.device_name.title"),
                    Some(crate::tr!("settings.device_name.description").into()),
                    reset,
                    compact,
                    cx,
                ),
                row.field(
                    Input::new(&self.device_name_input.state)
                        .placeholder(crate::tr!("settings.device_name.placeholder")),
                    cx,
                ),
                cx,
            )
            .debug_selector(|| "settings-device-name-row".into());
        row.render(layout)
    }

    pub(super) fn remote_attachment_limit_row(
        &self,
        overridden: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let reset = crate::settings_rows::reset_action(
            "reset-remote-attachment-limit",
            overridden,
            cx.listener(|this, _, window, cx| {
                DevicePreferences::set_remote_attachment_limit_mib(None, cx);
                let limit = remote_attachment_limit_value(cx);
                this.remote_attachment_limit_input.push(limit, window, cx);
            }),
        );
        let row = self.flip_row("remote-attachment-limit", cx);
        let layout = row
            .row(
                crate::settings_rows::row_labels(
                    crate::tr!("settings.remote_attachment_limit.title"),
                    Some(crate::tr!("settings.remote_attachment_limit.description").into()),
                    reset,
                    row.compact(),
                    cx,
                ),
                crate::widgets::input::NumberInput::new(&self.remote_attachment_limit_input.state)
                    .accessibility_label(crate::tr!("settings.remote_attachment_limit.title")),
                cx,
            )
            .debug_selector(|| "settings-remote-attachment-limit-row".into());
        row.render(layout)
    }

    pub(super) fn render_tcode_update_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.store.read(cx).tcode_update_status();
        let version = status.latest.unwrap_or_default();
        let release_url = status.release_url.unwrap_or_default();

        crate::material::overlay_popover("tcode-update-popover", cx)
            .p_3()
            .trigger(
                Button::new("tcode-update-available")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/download.svg"))
                    .label(crate::tr!(
                        "providers.tcode_update_available",
                        version = version
                    ))
                    .tooltip(crate::tr!("providers.tcode_update_aria")),
            )
            .content(move |_, _, cx| {
                let release_url = release_url.clone();
                v_flex()
                    // A reading measure for the message; gpui-base has no popover width.
                    .w(px(320.))
                    .gap_2()
                    .child(
                        div()
                            .text_title(cx)
                            .child(crate::tr!("providers.tcode_update_title")),
                    )
                    .child(
                        div()
                            .text_secondary(false, cx)
                            .child(crate::tr!("providers.tcode_update_message")),
                    )
                    .child(
                        Button::new("view-tcode-release")
                            .primary()
                            .small()
                            .label(crate::tr!("providers.view_release"))
                            .on_click(move |_, _, cx| cx.open_url(&release_url)),
                    )
            })
            .into_any_element()
    }
}
