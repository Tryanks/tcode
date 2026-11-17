use super::*;

impl SettingsPage {
    pub(super) fn render_computer_use(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let compact = self.window_state.read(cx).compact;
        let settings = self.store.read(cx).settings();
        let enabled_reset = crate::settings_rows::reset_action(
            "reset-cu-enabled",
            settings.computer_use.enabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_enabled(false), cx)
            }),
        );
        let allow_input_reset = crate::settings_rows::reset_action(
            "reset-cu-allow-input",
            !settings.computer_use.allow_input,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_allow_input(true), cx)
            }),
        );
        let show_agent_cursor_reset = crate::settings_rows::reset_action(
            "reset-cu-show-agent-cursor",
            !settings.computer_use.show_agent_cursor,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_show_agent_cursor(true), cx)
            }),
        );
        let rows = vec![
            self.toggle_row(
                "cu-enabled",
                crate::tr!("computer_use.enable.title"),
                crate::tr!("computer_use.enable.description"),
                settings.computer_use.enabled,
                enabled_reset,
                cx,
                WorkspaceStore::set_computer_use_enabled,
            ),
            self.image_mode_row(settings.computer_use.image_mode, cx),
            self.toggle_row(
                "cu-allow-input",
                crate::tr!("computer_use.allow_input.title"),
                crate::tr!("computer_use.allow_input.description"),
                settings.computer_use.allow_input,
                allow_input_reset,
                cx,
                WorkspaceStore::set_computer_use_allow_input,
            ),
            self.toggle_row(
                "cu-show-agent-cursor",
                crate::tr!("computer_use.show_agent_cursor.title"),
                crate::tr!("computer_use.show_agent_cursor.description"),
                settings.computer_use.show_agent_cursor,
                show_agent_cursor_reset,
                cx,
                WorkspaceStore::set_computer_use_show_agent_cursor,
            ),
        ];
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(
                v_flex()
                    .child(crate::material::group_caption(
                        crate::tr!("computer_use.section"),
                        cx,
                    ))
                    .child(crate::material::group(cx).children(rows))
                    .children(
                        self.store
                            .read(cx)
                            .harness_descriptions()
                            .iter()
                            .filter(|description| description.native_computer_use)
                            .map(|description| description.provider)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .map(|kind| {
                                div()
                                    .pt(cx.theme().tokens.spacing.sm)
                                    .pl(px(crate::material::CARD_INSET))
                                    .text_secondary(compact, cx)
                                    .child(crate::tr!(
                                        "providers.native_computer_use",
                                        name = kind.display_name()
                                    ))
                            }),
                    ),
            )
            .child(self.permissions_group(cx))
    }
}
