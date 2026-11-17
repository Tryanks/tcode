use super::*;

impl SettingsPage {
    pub(super) fn render_browser(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        let enabled_reset = crate::settings_rows::reset_action(
            "reset-browser-enabled",
            !settings.browser.enabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_browser_enabled(true), cx)
            }),
        );
        let allow_eval_reset = crate::settings_rows::reset_action(
            "reset-browser-allow-eval",
            !settings.browser.allow_evaluate,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_browser_allow_evaluate(true), cx)
            }),
        );
        let rows = vec![
            self.toggle_row(
                "browser-enabled",
                crate::tr!("browser.enable.title"),
                crate::tr!("browser.enable.description"),
                settings.browser.enabled,
                enabled_reset,
                cx,
                WorkspaceStore::set_browser_enabled,
            ),
            self.home_url_row(cx),
            self.toggle_row(
                "browser-allow-eval",
                crate::tr!("browser.allow_evaluate.title"),
                crate::tr!("browser.allow_evaluate.description"),
                settings.browser.allow_evaluate,
                allow_eval_reset,
                cx,
                WorkspaceStore::set_browser_allow_evaluate,
            ),
        ];
        v_flex().gap(crate::material::section_gap(cx)).child(
            v_flex()
                .child(crate::material::group_caption(
                    crate::tr!("browser.section"),
                    cx,
                ))
                .child(crate::material::group(cx).children(rows)),
        )
    }

    pub(super) fn home_url_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let overridden = self.store.read(cx).settings().browser.home_url.is_some();
        let reset = crate::settings_rows::reset_action(
            "reset-browser-home-url",
            overridden,
            cx.listener(|this, _, window, cx| {
                this.dispatch_settings(|store| store.set_browser_home_url(None), cx);
                this.home_url_input.push(String::new(), window, cx);
            }),
        );
        let row = self.flip_row("browser-home-url", cx);
        let layout = row.row(
            crate::settings_rows::row_labels(
                crate::tr!("browser.home_url.title"),
                Some(crate::tr!("browser.home_url.description").into()),
                reset,
                compact,
                cx,
            ),
            row.field(
                Input::new(&self.home_url_input.state)
                    .placeholder(crate::tr!("browser.home_url.placeholder")),
                cx,
            ),
            cx,
        );
        row.render(layout)
    }

    pub(super) fn image_mode_row(&self, mode: ImageMode, cx: &mut Context<Self>) -> AnyElement {
        let reset = crate::settings_rows::reset_action(
            "reset-cu-image-mode",
            mode != ImageMode::Auto,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_computer_use_image_mode(ImageMode::Auto),
                    cx,
                );
            }),
        );
        let label = match mode {
            ImageMode::Auto => crate::tr!("computer_use.image_mode.auto"),
            ImageMode::Always => crate::tr!("computer_use.image_mode.always"),
            ImageMode::Never => crate::tr!("computer_use.image_mode.never"),
        };
        let option = |value, label_key: &'static str, desc_key: &'static str| SelectRowOption {
            value,
            id: label_key.into(),
            label: crate::tr!(label_key).into_owned().into(),
            description: Some(crate::tr!(desc_key).into_owned().into()),
            selected: value == mode,
        };
        crate::settings_rows::select_row(
            "cu-image-mode-dropdown",
            "cu-image-mode-popover",
            "cu-image-mode-menu",
            260.,
            crate::tr!("computer_use.image_mode.title")
                .into_owned()
                .into(),
            Some(
                crate::tr!("computer_use.image_mode.description")
                    .into_owned()
                    .into(),
            ),
            label.into_owned().into(),
            vec![
                option(
                    ImageMode::Auto,
                    "computer_use.image_mode.auto",
                    "computer_use.image_mode.auto_desc",
                ),
                option(
                    ImageMode::Always,
                    "computer_use.image_mode.always",
                    "computer_use.image_mode.always_desc",
                ),
                option(
                    ImageMode::Never,
                    "computer_use.image_mode.never",
                    "computer_use.image_mode.never_desc",
                ),
            ],
            reset,
            self.flip_row("cu-image-mode-dropdown", cx),
            {
                let page = cx.entity();
                move |mode, _, cx| {
                    page.update(cx, |page, cx| {
                        page.dispatch_settings(|store| store.set_computer_use_image_mode(mode), cx)
                    })
                }
            },
            cx,
        )
    }
}
