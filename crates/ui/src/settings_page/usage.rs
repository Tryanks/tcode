use super::*;

impl SettingsPage {
    /// Show the rate-limit windows each enabled Codex / Claude Code profile
    /// reports; missing windows are not synthesized.
    pub(super) fn render_usage(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let store = self.store.read(cx);
        let profiles: Vec<_> = store
            .enabled_profiles()
            .into_iter()
            .filter(|profile| profile.supports_account_usage())
            .collect();
        let rows: Vec<(String, String, agent::ProviderKind, _, bool)> = profiles
            .iter()
            .map(|profile| {
                (
                    profile.id.clone(),
                    store.provider_profile_display_name(&profile.id),
                    profile.kind,
                    store.provider_usage(&profile.id),
                    store.usage_checking(&profile.id),
                )
            })
            .collect();
        let updated_at = rows
            .iter()
            .filter_map(|(_, _, _, usage, _)| usage.as_ref().map(|usage| usage.fetched_at))
            .max();
        let checking = rows.iter().any(|(_, _, _, _, checking)| *checking);
        let compact = self.window_state.read(cx).compact;

        let mut header = gpui_base::h_flex().w_full().items_center().gap_2().child(
            div()
                .flex_1()
                .pl(px(crate::material::CARD_INSET))
                .text_caption(cx)
                .child(crate::widgets::fade_text(crate::tr!("usage.section"))),
        );
        let updated = updated_at.map(|updated_at| {
            let ago = humanize_ago(
                self.store
                    .read(cx)
                    .host_now_secs(cx)
                    .saturating_sub(updated_at),
            );
            crate::tr!("usage.updated", when = ago).into_owned()
        });
        if let Some(updated) = crate::motion::shown("usage-updated", updated, SHOW, window, cx) {
            header = header.child(
                updated.fade(
                    div()
                        .text_secondary(compact, cx)
                        .child(crate::widgets::fade_text(updated.value.clone())),
                ),
            );
        }
        header = header.child(
            Button::new("refresh-usage")
                .ghost()
                .xsmall()
                .loading(checking)
                .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                .tooltip(crate::tr!("usage.refresh"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store
                        .update(cx, |store, _cx| store.refresh_provider_usage());
                })),
        );

        // Each card and the note carry the gap above them, so it comes and
        // goes with them.
        let mut section = v_flex().w_full().child(header);
        if let Some(none) = crate::motion::shown(
            "usage-no-profiles",
            rows.is_empty().then_some(()),
            SHOW,
            window,
            cx,
        ) {
            section = section.child(div().w_full().pt_3().child(none.fade(
                crate::material::grouped(
                    vec![self.usage_note_row(
                        crate::tr!("usage.no_profiles").into_owned(),
                        None,
                        cx,
                    )],
                    cx,
                ),
            )));
        }
        let cards = rows
            .into_iter()
            .map(|row| (SharedString::from(row.0.clone()), row));
        let cards = crate::motion::keyed(
            "usage-cards",
            cards,
            ROWS,
            window,
            cx,
            |_, (profile_id, name, kind, usage, checking), window, cx| {
                div()
                    .w_full()
                    .pt_3()
                    .child(self.usage_card(
                        profile_id,
                        name,
                        *kind,
                        usage.as_ref(),
                        *checking,
                        window,
                        cx,
                    ))
                    .into_any_element()
            },
        );
        section.child(cards.render(v_flex().w_full()))
    }

    /// One profile's usage: its header, then its windows, or why there are
    /// none.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn usage_card(
        &self,
        profile_id: &str,
        name: &str,
        kind: agent::ProviderKind,
        usage: Option<&tcode_core::usage::ProviderUsage>,
        checking: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let card_id = gpui::ElementId::from(SharedString::from(format!("usage-card-{profile_id}")));
        let plan = crate::motion::shown(
            (card_id.clone(), "plan"),
            usage.and_then(|u| u.plan.clone()),
            SHOW,
            window,
            cx,
        );
        let header = self.usage_card_header(profile_id, name, kind, plan, cx);
        let reported = usage.filter(|usage| usage.error.is_some() || !usage.windows.is_empty());
        let error = crate::motion::shown(
            (card_id.clone(), "error"),
            reported.and_then(|usage| usage.error.clone()),
            SHOW,
            window,
            cx,
        );
        let hairline =
            |cx: &App| crate::material::row_hairline(px(crate::material::CARD_INSET), cx);
        let slot = match reported {
            Some(_) => "windows",
            None if checking => "checking",
            None => "no-data",
        };
        let body = crate::motion::cross_fade(
            (card_id.clone(), "body"),
            slot,
            CHANGE,
            window,
            cx,
            |slot, window, cx| match *slot {
                "windows" => {
                    // The error fold draws its own hairline, and each window
                    // the one above it, so each line folds away with its row.
                    let error = match error.as_ref() {
                        Some(error) => {
                            error.fold(div().child(hairline(cx)).child(self.usage_note_row(
                                crate::tr!("usage.unavailable").into_owned(),
                                Some(error.value.clone()),
                                cx,
                            )))
                        }
                        None => div().into_any_element(),
                    };
                    let now = self.store.read(cx).host_now_secs(cx);
                    let windows = reported
                        .into_iter()
                        .flat_map(|usage| usage.windows.iter())
                        .map(|window| {
                            (
                                SharedString::from(crate::usage::window_label(window)),
                                window.clone(),
                            )
                        });
                    let windows = crate::motion::keyed(
                        (card_id.clone(), "windows"),
                        windows,
                        ROWS,
                        window,
                        cx,
                        |_, window, _, cx| {
                            crate::settings_rows::hairlined(
                                self.usage_window_row(window, now, cx),
                                cx,
                            )
                        },
                    );
                    v_flex()
                        .w_full()
                        .child(error)
                        .child(windows.render(v_flex().w_full()))
                        .into_any_element()
                }
                slot => v_flex()
                    .w_full()
                    .child(hairline(cx))
                    .child(self.usage_note_row(
                        if slot == "checking" {
                            crate::tr!("usage.checking").into_owned()
                        } else {
                            crate::tr!("usage.no_data").into_owned()
                        },
                        None,
                        cx,
                    ))
                    .into_any_element(),
            },
        );
        div()
            .id(card_id)
            .child(crate::material::grouped(vec![header, body], cx))
            .into_any_element()
    }

    pub(super) fn usage_card_header(
        &self,
        profile_id: &str,
        name: &str,
        kind: agent::ProviderKind,
        plan: Option<crate::motion::Shown<String>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let compact = self.window_state.read(cx).compact;
        crate::material::group_row(cx)
            .text_body(compact, cx)
            .child(crate::provider_card::profile_glyph(
                self.store.read(cx),
                kind,
                Some(profile_id),
            ))
            .child(
                div()
                    .flex_1()
                    .child(crate::widgets::fade_text(name.to_owned())),
            )
            .when_some(plan, |row, plan| {
                row.child(plan.fade(crate::material::semantic_chip(
                    crate::usage::plan_label(&plan.value),
                    cx.theme().muted,
                    muted,
                    cx,
                )))
            })
            .into_any_element()
    }

    pub(super) fn usage_window_row(
        &self,
        window: &tcode_core::usage::UsageWindow,
        now: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let fill = crate::usage::bar_color(window.used_percent, cx);
        let label = crate::usage::window_label(window);
        crate::material::group_row_stacked(cx)
            .child(
                gpui_base::h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_caption(cx)
                                    .child(crate::widgets::fade_text(label.clone())),
                            )
                            .when_some(
                                crate::usage::resets_label(window.resets_at, now),
                                |col, label| {
                                    col.child(div().text_secondary(compact, cx).child(label))
                                },
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_meta_mono(compact, cx)
                            .child(crate::usage::percent_label(window.used_percent)),
                    ),
            )
            .child(
                crate::widgets::progress::Progress::new(SharedString::from(format!(
                    "usage-window-{label}"
                )))
                .small()
                .color(fill)
                .value(window.used_percent),
            )
            .into_any_element()
    }

    pub(super) fn usage_note_row(
        &self,
        label: String,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        crate::material::group_row_stacked(cx)
            .text_secondary(compact, cx)
            .child(label)
            .children(detail)
            .into_any_element()
    }
}
