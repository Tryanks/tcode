use super::*;

impl HarnessDialog {
    pub(super) fn render_overview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let Some(description) = describe(store, &self.harness) else {
            return crate::material::empty_row(crate::tr!("composer.loading"), cx)
                .into_any_element();
        };
        let snapshot = store.provider_profile_snapshot(self.builtin_id());
        let missing = description.acp_agent_id.is_none()
            && snapshot.as_ref().is_some_and(|snapshot| {
                !snapshot.checking && snapshot.status.is_some() && !snapshot.installed
            });
        let mut column = v_flex().w_full().gap(crate::material::section_gap(cx));
        let banners = self.render_banners(&description, cx);
        if !banners.is_empty() {
            column = column.child(v_flex().w_full().gap_2().children(banners));
        }
        if missing {
            return column
                .child(self.render_install(&description, window, cx))
                .into_any_element();
        }
        if description.acp_agent_id.is_none() {
            if self.lists_accounts(cx) {
                column = column.child(self.render_accounts(&description, window, cx));
            } else {
                let profile = self.builtin_id().to_owned();
                column = column
                    .children(self.render_sign_in(&description, &profile, window, cx))
                    .child(self.render_models(&profile, window, cx));
            }
        }
        column = column.child(self.render_run(&description, window, cx));
        column = column.children(self.render_native_mcp(cx));
        if let Some(integrations) = self.render_integrations(&description, cx) {
            column = column.child(integrations);
        }
        if let HarnessId::Acp(id) = self.harness.clone() {
            let name = harness_name(self.store.read(cx), &self.harness);
            column = column.child(self.danger_row(
                "harness-remove-agent",
                crate::tr!("providers.harness.remove_title", name = name).into_owned(),
                crate::tr!("providers.harness.remove_body").into_owned(),
                crate::tr!("providers.harness.remove").into_owned(),
                cx.listener(move |this, _, window, cx| {
                    this.confirm_remove_agent(id.clone(), window, cx)
                }),
                cx,
            ));
        }
        column.into_any_element()
    }

    pub(super) fn render_accounts(
        &mut self,
        description: &HarnessDescription,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let profiles = harness_profiles(store, &self.harness);
        let builtin = self.builtin_id();
        let mut rows = Vec::new();
        for profile in &profiles {
            rows.push(self.account_row(description, profile, profile.id == builtin, cx));
        }
        // Another account lives in a config directory of its own.
        let account_login = description.account_login && description.config_dir_env.is_some();
        let endpoints = description.offers_endpoint_profiles();
        let build_actions = |cx: &mut Context<Self>| {
            let mut actions = h_flex().gap_1();
            if account_login {
                actions = actions.child(
                    Button::new("harness-add-account")
                        .debug_selector(|| "harness-add-account".into())
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label(crate::tr!("providers.harness.add_account"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.navigate(Page::AddAccount, window, cx)
                        })),
                );
            }
            if endpoints {
                actions = actions.child(
                    Button::new("harness-add-endpoint")
                        .debug_selector(|| "harness-add-endpoint".into())
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label(crate::tr!("providers.harness.add_endpoint"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.navigate(Page::AddEndpoint, window, cx)
                        })),
                );
            }
            actions
        };
        let compact = self.compact();
        let actions = build_actions(cx);
        v_flex()
            .w_full()
            .debug_selector(|| "harness-accounts".into())
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .child(crate::material::group_caption(
                        if endpoints {
                            crate::tr!("providers.harness.accounts")
                        } else {
                            crate::tr!("providers.harness.accounts_only")
                        }
                        .into_owned(),
                        cx,
                    ))
                    .when(!compact, |row| row.child(actions)),
            )
            .child(crate::material::grouped(rows, cx))
            .when(compact, |column| column.child(build_actions(cx).pt_2()))
            .into_any_element()
    }

    pub(super) fn account_row(
        &self,
        description: &HarnessDescription,
        profile: &ResolvedProfile,
        is_default: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let snapshot = store.provider_profile_snapshot(&profile.id);
        let endpoint = profile.is_endpoint_profile();
        let name = store.provider_profile_short_name(&profile.id);
        let detail = if endpoint {
            endpoint_summary(description, &profile.settings.env)
        } else {
            account_summary(snapshot.as_ref())
        };
        let signed_out = !endpoint
            && description.account_login
            && snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.installed
                    && snapshot
                        .auth
                        .as_ref()
                        .is_some_and(|auth| auth.status == AuthStatus::Unauthenticated)
            });
        let id = profile.id.clone();
        let open_id = id.clone();
        let login_id = id.clone();
        let enabled_id = id.clone();
        let kind = profile.kind;
        let label = name.clone();
        let tooltip_name = name.clone();
        let enabled = profile.settings.enabled;
        let body = crate::material::accessible_clickable(
            h_flex(),
            SharedString::from(format!("harness-account-{id}")),
            gpui::Role::Button,
            label,
            cx,
        )
        .debug_selector({
            let id = id.clone();
            move || format!("harness-account-{id}")
        })
        .flex_1()
        .min_w_0()
        .gap(cx.theme().tokens.spacing.md)
        .items_center()
        .cursor_pointer()
        .child(crate::provider_card::profile_glyph(store, kind, Some(&id)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_body(false, cx)
                                .child(crate::widgets::fade_text(name)),
                        )
                        .when(is_default, |row| {
                            row.child(crate::material::chip(
                                crate::tr!("providers.harness.default").into_owned(),
                                cx,
                            ))
                        })
                        .when(endpoint, |row| row.child(endpoint_chip(cx))),
                )
                .when(!detail.is_empty(), |column| {
                    column.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_secondary(false, cx)
                            .child(crate::widgets::fade_text(detail)),
                    )
                }),
        )
        .when(signed_out, |row| {
            row.child(
                Button::new(SharedString::from(format!("harness-sign-in-{login_id}")))
                    .primary()
                    .xsmall()
                    .label(crate::tr!("providers.login.sign_in"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        crate::provider_login_dialog::open(
                            this.store.clone(),
                            kind,
                            login_id.clone(),
                            window,
                            cx,
                        );
                    })),
            )
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            this.navigate(Page::Profile(open_id.clone()), window, cx)
        }));
        crate::material::group_row(cx)
            .child(body)
            .child(
                Switch::new(SharedString::from(format!("harness-account-enabled-{id}")))
                    .checked(enabled)
                    .tooltip(crate::tr!("providers.enable", name = tooltip_name))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.update_profile(
                            enabled_id.clone(),
                            ProfileSettingsPatch::SetEnabled { enabled: *checked },
                            cx,
                        );
                    })),
            )
            .into_any_element()
    }

    /// Who an account is signed in as, and how to sign it in or out.
    pub(super) fn render_sign_in(
        &mut self,
        description: &HarnessDescription,
        profile_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let snapshot = store.provider_profile_snapshot(profile_id);
        // Sign-in the CLI could not report is said by the banner over it.
        let unknown = snapshot.as_ref().is_some_and(|snapshot| {
            !snapshot.checking
                && snapshot.installed
                && snapshot
                    .auth
                    .as_ref()
                    .is_none_or(|auth| auth.status == AuthStatus::Unknown)
        });
        if unknown {
            return None;
        }
        let manages =
            description.account_login && store.provider_profile_supports_account_login(profile_id);
        let account = snapshot.as_ref().and_then(signed_in_account).cloned();
        let signed_in = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.auth.as_ref())
            .is_some_and(|auth| auth.status == AuthStatus::Authenticated);
        let kind = self.kind();
        let id = profile_id.to_owned();
        let row = if signed_in {
            let revealed = self.email_revealed.contains(profile_id);
            let label = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.auth.as_ref())
                .and_then(|auth| auth.label.clone())
                .filter(|label| !label.is_empty());
            let email = account.and_then(|auth| auth.email);
            let reveal_id = id.clone();
            let title: AnyElement = match email {
                Some(email) => Button::new(SharedString::from(format!("harness-email-{id}")))
                    .text()
                    .label(if revealed {
                        email
                    } else {
                        redact_email(&email)
                    })
                    .tooltip(if revealed {
                        crate::tr!("providers.hide_email")
                    } else {
                        crate::tr!("providers.reveal_email")
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.email_revealed.remove(&reveal_id) {
                            this.email_revealed.insert(reveal_id.clone());
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
                None => div()
                    .text_body(false, cx)
                    .child(crate::widgets::fade_text(crate::tr!(
                        "providers.status.authenticated"
                    )))
                    .into_any_element(),
            };
            let hint = (!manages)
                .then(|| description.sign_in_command.clone())
                .flatten()
                .map(|command| {
                    crate::tr!("providers.harness.switch_account", command = command).into_owned()
                });
            let sign_out_id = id.clone();
            crate::material::group_row(cx)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(title)
                                .when_some(label, |row, label| {
                                    row.child(crate::material::chip(label, cx))
                                }),
                        )
                        .when_some(hint, |column, hint| {
                            column.child(div().text_secondary(false, cx).child(hint))
                        }),
                )
                .when(manages, |row| {
                    row.child(
                        Button::new("harness-sign-out")
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("providers.login.sign_out"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_sign_out(sign_out_id.clone(), window, cx)
                            })),
                    )
                })
                .into_any_element()
        } else {
            let detail = match &snapshot {
                Some(snapshot) if !snapshot.checking && snapshot.status.is_some() => {
                    crate::provider_status::snapshot_detail(description, snapshot)
                }
                _ => crate::tr!("providers.status.checking").into_owned(),
            };
            let detail = if manages {
                crate::tr!("providers.harness.sign_in_help").into_owned()
            } else {
                detail
            };
            crate::material::group_row(cx)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .text_body(false, cx)
                                .child(crate::tr!("providers.harness.signed_out")),
                        )
                        .child(div().text_secondary(false, cx).child(detail)),
                )
                .when(manages, |row| {
                    row.child(
                        Button::new("harness-sign-in")
                            .debug_selector(|| "harness-sign-in".into())
                            .primary()
                            .xsmall()
                            .label(crate::tr!("providers.login.sign_in"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                crate::provider_login_dialog::open(
                                    this.store.clone(),
                                    kind,
                                    id.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
                })
                .into_any_element()
        };
        Some(self.group(
            Some(crate::tr!("providers.harness.account").into_owned()),
            vec![row],
            cx,
        ))
    }
}
