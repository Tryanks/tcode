use super::*;

impl SettingsPage {
    /// Settings → Harnesses: native providers and installed ACP agents share one
    /// bordered list. The marketplace lives behind the Add agent dialog.
    pub(super) fn render_providers(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let checked_at = self.store.read(cx).providers_checked_at();
        let checking = self.store.read(cx).providers_checking();
        let run = self.store.read(cx).provider_update_run();
        let automatic = self.store.read(cx).automatic_provider_updates();
        let compact = self.window_state.read(cx).compact;

        let mut header = gpui_base::h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .pl(px(crate::material::CARD_INSET))
                    .text_caption(cx)
                    .child(crate::widgets::fade_text(crate::tr!(
                        "settings.providers_section"
                    ))),
            );
        let checked = checked_at.map(|checked_at| {
            let ago = humanize_ago(
                self.store
                    .read(cx)
                    .host_now_secs(cx)
                    .saturating_sub(checked_at),
            );
            crate::tr!("providers.checked", when = ago).into_owned()
        });
        if let Some(checked) = crate::motion::shown("providers-checked", checked, SHOW, window, cx)
        {
            header = header.child(
                checked.fade(
                    div()
                        .text_secondary(compact, cx)
                        .child(crate::widgets::fade_text(checked.value.clone())),
                ),
            );
        }
        let update_available = self.store.read(cx).tcode_update_status().update_available;
        if let Some(update) = crate::motion::shown(
            "tcode-update",
            update_available.then_some(()),
            SHOW,
            window,
            cx,
        ) {
            header = header.child(update.fade(self.render_tcode_update_popover(cx)));
        }
        header = header.child(
            Button::new("add-acp-agent")
                .outline()
                .xsmall()
                .icon(IconName::Plus)
                .label(crate::tr!("providers.acp.add_agent").into_owned())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_acp_dialog(window, cx);
                })),
        );
        if let Some(update_all) = crate::motion::shown(
            "update-all-providers",
            (!automatic.is_empty()).then(|| automatic.clone()),
            SHOW,
            window,
            cx,
        ) {
            let automatic = update_all.value.clone();
            header = header.child(
                update_all.fade(
                    Button::new("update-all-providers")
                        .outline()
                        .xsmall()
                        .loading(run.is_some())
                        .label(crate::tr!("providers.update_all"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store
                                .update(cx, |store, _cx| store.update_providers(automatic.clone()));
                        })),
                ),
            );
        }
        header = header.child(
            Button::new("refresh-providers")
                .ghost()
                .xsmall()
                .loading(checking)
                .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                .tooltip(crate::tr!("providers.refresh"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store.update(cx, |store, _cx| {
                        store.refresh_provider_status();
                        store.check_provider_versions();
                    });
                })),
        );

        let update_checks_disabled = self
            .store
            .read(cx)
            .settings()
            .provider_update_checks_disabled;
        let provider_updates_reset = crate::settings_rows::reset_action(
            "reset-provider-update-checks",
            update_checks_disabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_provider_update_checks_disabled(false), cx)
            }),
        );
        let updates = vec![self.toggle_row(
            "provider-update-checks",
            crate::tr!("settings.provider_updates.title"),
            crate::tr!("settings.provider_updates.description"),
            // Stored inverted: checked = enabled.
            !update_checks_disabled,
            provider_updates_reset,
            cx,
            |store, checked| store.set_provider_update_checks_disabled(!checked),
        )];

        // One row per harness: each native one this client sees, then each
        // installed ACP agent.
        let harnesses: Vec<(SharedString, HarnessId)> = {
            let store = self.store.read(cx);
            let profiles = store.all_provider_profiles();
            agent::ProviderKind::NATIVE
                .into_iter()
                .filter(|kind| profiles.iter().any(|profile| profile.kind == *kind))
                .map(HarnessId::Native)
                .chain(
                    store
                        .settings_installed_acp_agents()
                        .into_iter()
                        .map(|agent| HarnessId::Acp(agent.id)),
                )
                .map(|harness| (SharedString::from(harness.key()), harness))
                .collect()
        };
        let page = cx.entity().downgrade();
        let store = self.store.clone();
        let provider_rows = crate::settings_rows::grouped_keyed(
            "harness-rows",
            harnesses,
            window,
            cx,
            |_, harness, _, cx| {
                let page = page.clone();
                let open = harness.clone();
                crate::provider_card::harness_row(
                    &store,
                    harness,
                    move |_, window, cx| {
                        _ = page.update(cx, |page, cx| page.open_harness(open.clone(), window, cx));
                    },
                    cx,
                )
            },
        );

        let mut section = v_flex().w_full().gap_3().child(header);
        if let Some(shown) = crate::motion::shown("provider-update-run", run, SHOW, window, cx) {
            let run = &shown.value;
            section = section.child(
                shown.fold(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .px(px(crate::material::CARD_INSET))
                        .child(
                            div().text_secondary(compact, cx).child(
                                crate::tr!(
                                    "providers.update_progress",
                                    provider = run
                                        .current
                                        .map(|provider| provider.display_name())
                                        .unwrap_or_default(),
                                    completed = run.completed,
                                    total = run.total
                                )
                                .into_owned(),
                            ),
                        )
                        .child(
                            crate::widgets::progress::Progress::new("provider-update-progress")
                                .value(100. * run.completed as f32 / run.total.max(1) as f32),
                        ),
                ),
            );
        }
        if let Some(error) = self.store.read(cx).login_shell_unread() {
            section = section.child(
                crate::material::notice(cx.theme().warning, cx)
                    .debug_selector(|| "providers-login-shell-unread".into())
                    .child(div().flex_1().min_w_0().text_secondary(compact, cx).child(
                        crate::tr!("providers.login_shell_unread", error = error).into_owned(),
                    )),
            );
        }
        if let Some(error) = self.store.read(cx).tcode_update_status().check_error {
            section = section.child(
                crate::material::notice(cx.theme().warning, cx)
                    .debug_selector(|| "providers-tcode-update-unchecked".into())
                    .child(div().flex_1().min_w_0().text_secondary(compact, cx).child(
                        crate::tr!("providers.tcode_update_unchecked", error = error).into_owned(),
                    )),
            );
        }
        section = section.child(crate::material::group(cx).child(provider_rows));
        // The update toggle follows the cards it governs; above them its caption
        // would stack straight under the page's own header row.
        section.child(div().pt(crate::material::section_gap(cx)).child(
            crate::settings_rows::settings_group(
                crate::tr!("settings.updates_section"),
                updates,
                cx,
            ),
        ))
    }
}
