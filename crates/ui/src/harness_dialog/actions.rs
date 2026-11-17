use super::*;

impl HarnessDialog {
    /// Add the account or endpoint profile the page's form describes.
    pub(super) fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = |this: &Self, field: Field, cx: &App| {
            this.fields
                .get(&field)
                .map(|(input, _)| input.read(cx).value().trim().to_string())
                .unwrap_or_default()
        };
        let kind = self.kind();
        let inherit = self.inherit_config;
        match self.page {
            Page::AddEndpoint => {
                let (name, base_url, model, key) = (
                    value(self, Field::AddName, cx),
                    value(self, Field::AddBaseUrl, cx),
                    value(self, Field::AddModel, cx),
                    value(self, Field::AddKey, cx),
                );
                if base_url.is_empty() || key.is_empty() {
                    self.add_error =
                        Some(crate::tr!("providers.harness.endpoint_required").into_owned());
                    cx.notify();
                    return;
                }
                self.store.update(cx, |store, _| {
                    store.create_third_party_profile(
                        kind,
                        name,
                        base_url,
                        Some(model).filter(|model| !model.is_empty()),
                        key,
                        inherit,
                    )
                });
                self.navigate(Page::Overview, window, cx);
            }
            Page::AddAccount => {
                let name = value(self, Field::AddName, cx);
                if self.adding || name.is_empty() {
                    return;
                }
                let create = self.store.update(cx, |store, cx| {
                    store.create_account_profile(kind, name, inherit, cx)
                });
                self.adding = true;
                self.add_error = None;
                cx.notify();
                cx.spawn_in(window, async move |this, cx| {
                    let response = create.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.adding = false;
                        match response {
                            Ok(tcode_protocol::CommandResponse::ProfileId(profile_id)) => {
                                this.navigate(Page::Profile(profile_id.clone()), window, cx);
                                crate::provider_login_dialog::show(
                                    this.store.clone(),
                                    kind,
                                    profile_id,
                                    window,
                                    cx,
                                );
                            }
                            Ok(other) => {
                                log::error!("unexpected create-account response: {other:?}");
                                this.add_error = Some(
                                    crate::tr!("providers.account.create_failed").into_owned(),
                                );
                            }
                            Err(error) => {
                                this.add_error = Some(match error.code.as_str() {
                                    "profile_exists" => {
                                        crate::tr!("providers.account.profile_exists").into_owned()
                                    }
                                    _ => crate::tr!(
                                        "providers.account.create_failed_reason",
                                        reason = error.message
                                    )
                                    .into_owned(),
                                });
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            _ => {}
        }
    }

    pub(super) fn manage_plugins(
        &self,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.embedded {
            window.close_dialog(cx);
        }
        cx.emit(ManagePlugins(profile_id));
    }

    pub(super) fn confirm_delete(
        &self,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let endpoint = self.is_endpoint_profile(cx);
        let (title, body) = delete_confirmation(store.read(cx), &profile_id, self.kind(), endpoint);
        let dialog = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let profile_id = profile_id.clone();
            let dialog = dialog.clone();
            alert
                .title(title.clone())
                .description(body.clone())
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(if endpoint {
                            crate::tr!("providers.harness.delete_endpoint")
                        } else {
                            crate::tr!("providers.harness.delete_account")
                        })
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let profile_id = profile_id.clone();
                    store.update(cx, |store, _| store.delete_profile(profile_id));
                    window.close_dialog(cx);
                    _ = dialog.update(cx, |dialog, cx| {
                        dialog.navigate(Page::Overview, window, cx);
                    });
                    true
                })
        });
    }

    pub(super) fn confirm_remove_agent(
        &self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let name = harness_name(store.read(cx), &self.harness);
        let dialog = cx.entity().downgrade();
        let embedded = self.embedded;
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let id = id.clone();
            let dialog = dialog.clone();
            alert
                .title(crate::tr!("providers.harness.remove_title", name = name).into_owned())
                .description(crate::tr!("providers.harness.remove_body"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("providers.acp.remove"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let id = id.clone();
                    store.update(cx, |store, _| store.remove_acp_agent(id));
                    if embedded {
                        window.close_dialog(cx);
                        _ = dialog.update(cx, |_, cx| cx.emit(gpui::DismissEvent));
                    } else {
                        window.close_all_dialogs(cx);
                    }
                    true
                })
        });
    }

    pub(super) fn confirm_sign_out(
        &self,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let name = store.read(cx).provider_profile_display_name(&profile_id);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let profile_id = profile_id.clone();
            alert
                .title(
                    crate::tr!("providers.login.sign_out_confirm_title", name = name).into_owned(),
                )
                .description(crate::tr!("providers.login.sign_out_confirm_body"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("providers.login.sign_out"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let profile_id = profile_id.clone();
                    store.update(cx, |store, _| store.logout_provider(profile_id));
                    true
                })
        });
    }
}
