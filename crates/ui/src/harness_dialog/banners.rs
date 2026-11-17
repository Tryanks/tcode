use super::*;

impl HarnessDialog {
    pub(super) fn render_banners(
        &self,
        description: &HarnessDescription,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut banners = Vec::new();
        let store = self.store.read(cx);
        let builtin = self.builtin_id();
        let theme = cx.theme().clone();
        let enabled = match &self.harness {
            HarnessId::Native(_) => harness_profiles(store, &self.harness)
                .iter()
                .any(|profile| profile.settings.enabled),
            HarnessId::Acp(id) => store
                .installed_acp_agent(id)
                .is_some_and(|agent| agent.enabled),
        };
        let snapshot = store.provider_profile_snapshot(builtin);
        let failed = snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.installed && snapshot.diagnostic == Some(ProviderProbeDiagnostic::FailedCli)
        });
        if failed {
            let snapshot = snapshot.clone().unwrap_or_default();
            let binary = description.binary_name.clone().unwrap_or_default();
            let command = format!("{} --version", snapshot.binary.clone().unwrap_or(binary));
            let output = snapshot.message.clone().unwrap_or_default();
            let diagnostic = format!("{command}\n{output}").trim().to_owned();
            banners.push(
                crate::material::notice(theme.danger, cx)
                    .debug_selector(|| "harness-banner-failed".into())
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_body(false, cx)
                                    .child(crate::tr!("providers.harness.failed")),
                            )
                            .child(div().text_code(cx).child(command))
                            .when(!output.is_empty(), |body| {
                                body.child(div().text_code(cx).child(output))
                            })
                            .when_some(description.doctor_command.clone(), |body, doctor| {
                                body.child(
                                    div().text_secondary(false, cx).child(
                                        crate::tr!("providers.harness.doctor", command = doctor)
                                            .into_owned(),
                                    ),
                                )
                            }),
                    )
                    .child(
                        Button::new("harness-copy-diagnostic")
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("providers.harness.copy_diagnostic"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(diagnostic.clone()))
                            }),
                    )
                    .into_any_element(),
            );
        }
        if let Some(detail) = snapshot.as_ref().and_then(|snapshot| {
            crate::provider_status::outdated_detail(&harness_name(store, &self.harness), snapshot)
        }) {
            banners.push(
                crate::material::notice(theme.warning, cx)
                    .debug_selector(|| "harness-banner-outdated".into())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .child(detail),
                    )
                    .into_any_element(),
            );
        }
        let attention = snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.status == Some(ProviderStatusKind::Warning)
                && snapshot.diagnostic == Some(ProviderProbeDiagnostic::IndeterminateAuth)
        });
        if attention && !self.lists_accounts(cx) {
            let snapshot = snapshot.clone().unwrap_or_default();
            banners.push(
                crate::material::notice(theme.warning, cx)
                    .debug_selector(|| "harness-banner-attention".into())
                    .child(div().flex_1().min_w_0().text_secondary(false, cx).child(
                        crate::provider_status::snapshot_detail(description, &snapshot),
                    ))
                    .into_any_element(),
            );
        }
        let shared_credentials = harness_profiles(store, &self.harness)
            .iter()
            .any(|profile| {
                store
                    .provider_profile_snapshot(&profile.id)
                    .is_some_and(|snapshot| {
                        snapshot.diagnostic == Some(ProviderProbeDiagnostic::SharedCredentials)
                    })
            });
        if shared_credentials {
            banners.push(
                crate::material::notice(theme.warning, cx)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .child(crate::tr!("providers.status.shared_credentials")),
                    )
                    .into_any_element(),
            );
        }
        if let HarnessId::Native(kind) = &self.harness
            && let Some(version) = store.provider_version_status(*kind)
            && (version.update_available || version.updating)
        {
            let kind = *kind;
            let latest = version.latest.clone().unwrap_or_default();
            let command = version.update_command.clone();
            let automatic = command.is_some() && !version.update_requires_terminal;
            let updating = version.updating;
            let mut banner = crate::material::notice(theme.info, cx)
                .debug_selector(|| "harness-banner-update".into())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_body(false, cx)
                        .child(if updating {
                            crate::tr!("providers.harness.updating", version = latest).into_owned()
                        } else {
                            crate::tr!("providers.harness.update_available", version = latest)
                                .into_owned()
                        }),
                );
            if automatic {
                banner = banner.child(
                    Button::new("harness-update")
                        .primary()
                        .xsmall()
                        .loading(updating)
                        .label(crate::tr!("providers.update_now"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store
                                .update(cx, |store, _| store.update_providers(vec![kind]))
                        })),
                );
            }
            let mut column = v_flex().w_full().gap_2().child(banner);
            if let Some(command) = command.filter(|_| !automatic) {
                column = column.child(self.command_line("harness-update-command", command, cx));
            }
            banners.push(column.into_any_element());
        }
        if let HarnessId::Native(kind) = &self.harness
            && let Some(error) = self
                .store
                .read(cx)
                .provider_version_status(*kind)
                .and_then(|version| version.latest_error)
        {
            banners.push(
                crate::material::notice(theme.warning, cx)
                    .debug_selector(|| "harness-banner-latest-unknown".into())
                    .child(div().flex_1().min_w_0().text_secondary(false, cx).child(
                        crate::tr!("providers.harness.latest_unknown", error = error).into_owned(),
                    ))
                    .into_any_element(),
            );
        }
        if let HarnessId::Native(kind) = &self.harness {
            let problems = self.store.read(cx).model_problems(*kind).join("\n");
            if !problems.is_empty() {
                banners.push(
                    crate::material::notice(theme.warning, cx)
                        .debug_selector(|| "harness-banner-catalog-problems".into())
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .text_body(false, cx)
                                        .child(crate::tr!("providers.harness.catalog_problems")),
                                )
                                .child(div().text_secondary(false, cx).child(problems)),
                        )
                        .into_any_element(),
                );
            }
        }
        if let Some(latest) = description.registry_update.clone()
            && let HarnessId::Acp(id) = &self.harness
        {
            let id = id.clone();
            let installing = self
                .store
                .read(cx)
                .acp_marketplace_items()
                .iter()
                .any(|item| item.id == id && item.installing);
            banners.push(
                crate::material::notice(theme.info, cx)
                    .debug_selector(|| "harness-banner-registry-update".into())
                    .child(
                        div().flex_1().min_w_0().text_body(false, cx).child(
                            crate::tr!("providers.harness.registry_update", version = latest)
                                .into_owned(),
                        ),
                    )
                    .child(
                        Button::new("harness-registry-update")
                            .primary()
                            .xsmall()
                            .loading(installing)
                            .label(crate::tr!("providers.update_now"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = id.clone();
                                this.store
                                    .update(cx, |store, _| store.install_acp_agent(id))
                            })),
                    )
                    .into_any_element(),
            );
        }
        if !enabled {
            banners.push(
                crate::material::notice(theme.warning, cx)
                    .debug_selector(|| "harness-banner-disabled".into())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_body(false, cx)
                            .child(crate::tr!("providers.harness.disabled")),
                    )
                    .child(
                        Button::new("harness-enable")
                            .xsmall()
                            .label(crate::tr!("providers.harness.enable"))
                            .on_click(cx.listener(|this, _, _, cx| this.set_enabled(true, cx))),
                    )
                    .into_any_element(),
            );
        }
        banners
    }

    pub(crate) fn set_enabled(&self, enabled: bool, cx: &mut App) {
        set_harness_enabled(&self.store, &self.harness, enabled, cx);
    }

    /// A command to run in a terminal, with its copy button.
    pub(super) fn command_line(
        &self,
        id: &'static str,
        command: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copied = self.copied.is(id);
        let copy = command.clone();
        h_flex()
            .w_full()
            .gap_1()
            .items_center()
            .rounded(crate::material::radius_input(cx))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted)
            .pl_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_code(cx)
                    .child(crate::widgets::fade_text(command)),
            )
            .child(crate::widgets::copy::copy_button(
                id,
                copied,
                self.compact(),
                cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    this.copied
                        .mark(id.to_owned(), |this: &mut Self| &mut this.copied, cx);
                }),
                cx,
            ))
            .into_any_element()
    }

    /// The whole overview of a harness whose CLI the host cannot find: how
    /// to install it, or where it already is.
    pub(super) fn render_install(
        &mut self,
        description: &HarnessDescription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let binary = description.binary_name.clone().unwrap_or_default();
        let mut rows = v_flex()
            .w_full()
            .gap_3()
            .debug_selector(|| "harness-install".into());
        rows = rows.child(
            div()
                .text_body(false, cx)
                .child(crate::tr!("providers.harness.not_found", binary = binary).into_owned()),
        );
        if let Some(command) = description.install_command.clone() {
            rows = rows
                .child(
                    div()
                        .text_caption(cx)
                        .child(crate::tr!("providers.harness.install")),
                )
                .child(self.command_line("harness-install-command", command, cx))
                .child(
                    div()
                        .text_secondary(false, cx)
                        .child(crate::tr!("providers.harness.install_help")),
                );
        }
        let input = self.input(Field::Binary, window, cx);
        rows.child(self.group(
            None,
            vec![
                    self.row(
                        crate::tr!("providers.harness.elsewhere").into_owned(),
                        None,
                        Some(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    div().flex_1().min_w_0().child(
                                        Input::new(&input).placeholder(crate::tr!(
                                            "providers.harness.binary_path"
                                        )),
                                    ),
                                )
                                .child(
                                    Button::new("harness-install-recheck")
                                        .outline()
                                        .small()
                                        .label(crate::tr!("providers.harness.recheck"))
                                        .on_click(cx.listener(|this, _, _, cx| this.recheck(cx))),
                                )
                                .into_any_element(),
                        ),
                        cx,
                    ),
                ],
            cx,
        ))
        .into_any_element()
    }
}
