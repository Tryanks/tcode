use super::*;

impl PluginsSettingsPanel {
    /// The list of Harnesses: the first page on a compact window, and what
    /// shows when none is managed.
    pub(super) fn overview(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let harnesses = self.harnesses(cx);
        let rows: Vec<AnyElement> = harnesses
            .iter()
            .map(|kind| {
                let kind = *kind;
                let accounts = self.accounts(kind, cx);
                let installed = rows(&accounts, false, PluginScope::User).len();
                let id = format!(
                    "plugins-harness-{}",
                    tcode_core::settings::provider_key(kind)
                );
                let subtitle = crate::tr!(
                    "providers.plugins.harness_summary",
                    accounts = accounts.len(),
                    plugins = installed
                );
                material::list_row(
                    SharedString::from(id.clone()),
                    kind.display_name().into(),
                    cx,
                )
                .debug_selector(move || id.clone())
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_body(self.touch, cx).child(kind.display_name()))
                        .child(
                            div()
                                .text_secondary(self.touch, cx)
                                .child(subtitle.into_owned()),
                        ),
                )
                .child(Icon::new(IconName::ChevronRight).text_color(cx.theme().muted_foreground))
                .on_click(cx.listener(move |this, _, _, cx| this.open(Page::Harness, kind, cx)))
                .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        v_flex()
            .w_full()
            .gap(cx.theme().tokens.spacing.lg)
            .when(!self.touch, |page| {
                page.child(material::group_caption(
                    crate::tr!("settings.plugins_section"),
                    cx,
                ))
            })
            .child(if empty {
                material::grouped(
                    vec![note_row(
                        crate::tr!("providers.plugins.no_providers").into_owned(),
                        cx,
                    )],
                    cx,
                )
                .into_any_element()
            } else {
                material::grouped(rows, cx).into_any_element()
            })
            .children(self.other_harnesses(cx))
            .into_any_element()
    }

    /// One honest line for each Harness with an account whose plugins Tcode
    /// does not manage.
    pub(super) fn other_harnesses(&self, cx: &App) -> Option<AnyElement> {
        let profiles = self.store.read(cx).enabled_profiles();
        let rows: Vec<AnyElement> = ProviderKind::NATIVE
            .into_iter()
            .filter(|kind| profiles.iter().any(|profile| profile.kind == *kind))
            .filter_map(|kind| {
                let unmanaged = plugins_of(self.store.read(cx), kind).unmanaged?;
                let name = kind.display_name();
                let line = match unmanaged {
                    UnmanagedPluginsKind::EditorOnly => {
                        crate::tr!("providers.plugins.unmanaged.editor_only", name = name)
                    }
                };
                let id = format!("plugins-other-{}", tcode_core::settings::provider_key(kind));
                Some(
                    material::group_row_stacked(cx)
                        .debug_selector(move || id.clone())
                        .gap_0p5()
                        .child(div().text_body(self.touch, cx).child(name))
                        .child(
                            div()
                                .text_secondary(self.touch, cx)
                                .child(line.into_owned()),
                        )
                        .into_any_element(),
                )
            })
            .collect();
        (!rows.is_empty()).then(|| {
            v_flex()
                .w_full()
                .child(material::group_caption(
                    crate::tr!("providers.plugins.other_harnesses"),
                    cx,
                ))
                .child(material::grouped(rows, cx))
                .into_any_element()
        })
    }

    pub(super) fn back_bar(
        &self,
        label: String,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .child(
                Button::new("plugins-back")
                    .debug_selector(|| "plugins-back".into())
                    .ghost()
                    .when(!compact, |button| button.small())
                    .icon(IconName::ChevronLeft)
                    .label(label)
                    .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
            )
            .into_any_element()
    }

    pub(super) fn harness_tabs(&self, kind: ProviderKind, cx: &mut Context<Self>) -> AnyElement {
        let tabs = self
            .harnesses(cx)
            .into_iter()
            .map(|harness| {
                let installed = rows(&self.accounts(harness, cx), false, PluginScope::User).len();
                let key = tcode_core::settings::provider_key(harness);
                TabItem::new(
                    SharedString::from(format!("plugins-tab-{key}")),
                    harness.display_name(),
                )
                .count(installed)
                .selected(harness == kind)
                .on_click(cx.listener(move |this, _, _, cx| this.open(Page::Harness, harness, cx)))
            })
            .collect::<Vec<_>>();
        TabBar::new("plugins-harness-tabs")
            .underline()
            .w_full()
            .aria_label(crate::tr!("settings.plugins"))
            .items(tabs)
            .into_any_element()
    }

    pub(super) fn harness_page(
        &mut self,
        kind: ProviderKind,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let management = plugins_of(self.store.read(cx), kind);
        let accounts = self.accounts(kind, cx);
        let columns: Vec<Account> = if accounts.len() > MAX_COLUMNS && !compact {
            let shown = self
                .column
                .as_ref()
                .and_then(|id| accounts.iter().find(|account| account.id == *id))
                .or(accounts.first())
                .cloned();
            shown.into_iter().collect()
        } else {
            accounts.clone()
        };
        let user_rows = rows(&columns, false, PluginScope::User);
        let managed_rows = rows(&columns, false, PluginScope::Managed);
        let has_project = management.scopes.contains(&PluginScope::Project);
        let project = has_project.then(|| self.project(cx)).flatten();

        let mut page = v_flex().w_full().gap(if compact {
            cx.theme().tokens.spacing.lg
        } else {
            material::section_gap(cx)
        });
        if self.touch {
            page = page.child(self.back_bar(
                crate::tr!("settings.plugins").into_owned(),
                self.touch,
                cx,
            ));
        } else {
            page = page.child(self.harness_tabs(kind, cx));
        }
        page = page.child(self.harness_header(kind, &accounts, compact, window, cx));
        if accounts.len() > MAX_COLUMNS && !compact {
            page = page.child(self.column_switcher(&accounts, &columns, cx));
        }
        page = page.children(self.failures(&accounts, kind, cx));

        // User scope: one switch per account.
        let caption = crate::tr!("providers.plugins.user_scope").into_owned();
        let note = (!management.supports(PluginActionKind::Enable)).then(|| {
            crate::tr!(
                "providers.plugins.no_enablement",
                name = kind.display_name()
            )
            .into_owned()
        });
        let listing = columns.iter().all(|account| {
            account.user.is_none()
                || (account.loading()
                    && account
                        .user
                        .as_ref()
                        .is_some_and(|catalog| catalog.entries.is_empty()))
        });
        let body = if user_rows.is_empty() {
            if listing {
                note_card(crate::tr!("providers.plugins.listing").into_owned(), cx)
            } else if columns
                .iter()
                .any(|account| account.user.as_ref().is_some_and(stopped))
            {
                note_card(crate::tr!("providers.plugins.stopped").into_owned(), cx)
            } else {
                self.empty_user(kind, &accounts, cx)
            }
        } else {
            self.plugin_rows(
                &columns,
                user_rows,
                false,
                PluginScope::User,
                kind,
                compact,
                window,
                cx,
            )
        };
        page = page.child(self.section(caption, None, &columns, compact, body, note, cx));

        if has_project {
            page = page.child(self.project_section(kind, &columns, project, compact, window, cx));
        }
        if !managed_rows.is_empty() {
            let body = self.plugin_rows(
                &columns,
                managed_rows,
                false,
                PluginScope::Managed,
                kind,
                compact,
                window,
                cx,
            );
            page = page.child(self.section(
                crate::tr!("providers.plugins.managed_scope").into_owned(),
                None,
                &columns,
                compact,
                body,
                None,
                cx,
            ));
        }
        if management.marketplaces {
            page = page.child(self.marketplaces(&columns, &accounts, compact, cx));
        }
        if !self.touch {
            page = page.children(self.other_harnesses(cx));
        }
        page.into_any_element()
    }

    pub(super) fn harness_header(
        &self,
        kind: ProviderKind,
        accounts: &[Account],
        compact: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let chips: Vec<_> = accounts
            .iter()
            .map(|account| {
                let state: AnyElement = if account.loading() {
                    Spinner::new().xsmall().color(muted).into_any_element()
                } else if account.failure().is_some() {
                    Icon::new(IconName::TriangleAlert)
                        .xsmall()
                        .text_color(cx.theme().danger)
                        .into_any_element()
                } else {
                    div().into_any_element()
                };
                let id = format!("plugins-account-{}", account.id);
                material::chip_surface(cx.theme().secondary, cx.theme().foreground, cx)
                    .debug_selector(move || id.clone())
                    .child(material::status_dot(account.color))
                    .child(crate::widgets::fade_text(account.name.clone()))
                    .child(state)
            })
            .collect();
        let listing: Vec<String> = accounts
            .iter()
            .filter(|account| account.loading())
            .map(|account| account.id.clone())
            .collect();
        let refresh = if listing.is_empty() {
            Button::new("plugins-refresh")
                .debug_selector(|| "plugins-refresh".into())
                .ghost()
                .small()
                .icon(IconName::RotateCcw)
                .tooltip(crate::tr!(
                    "providers.plugins.refresh",
                    name = kind.display_name()
                ))
                .on_click(cx.listener(move |this, _, _, cx| this.relist(kind, cx)))
        } else {
            Button::new("plugins-stop-listing")
                .debug_selector(|| "plugins-stop-listing".into())
                .ghost()
                .small()
                .icon(IconName::Square)
                .tooltip(crate::tr!(
                    "providers.plugins.stop_listing",
                    name = kind.display_name()
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.store.update(cx, |store, _| {
                        for profile_id in &listing {
                            store.stop_plugin_listing(profile_id.clone());
                        }
                    })
                }))
        };
        let installs = plugins_of(self.store.read(cx), kind).supports(PluginActionKind::Install);
        let browse = installs.then(|| {
            self.install_button("plugins-browse", kind, accounts, compact, cx)
                .debug_selector(|| "plugins-browse".into())
        });
        let description = div().text_secondary(self.touch, cx).child(
            if home_path_of(self.store.read(cx), kind) {
                crate::tr!("providers.plugins.where_kept").into_owned()
            } else {
                crate::tr!(
                    "providers.plugins.where_kept_shared",
                    name = kind.display_name()
                )
                .into_owned()
            },
        );
        if compact {
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .children(browse.map(|b| div().flex_1().child(b)))
                        .child(refresh),
                )
                .child(description)
                .child(h_flex().w_full().flex_wrap().gap_1p5().children(chips))
                .into_any_element()
        } else {
            h_flex()
                .w_full()
                .items_start()
                .gap_3()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .child(description)
                        .child(h_flex().w_full().flex_wrap().gap_1p5().children(chips)),
                )
                .child(refresh)
                .children(browse)
                .into_any_element()
        }
    }

    pub(super) fn column_switcher(
        &self,
        accounts: &[Account],
        columns: &[Account],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = columns.first().map(|account| account.id.clone());
        let items = accounts.iter().map(|account| {
            let id = account.id.clone();
            TabItem::new(
                SharedString::from(format!("plugins-column-{id}")),
                account.name.clone(),
            )
            .selected(shown.as_ref() == Some(&account.id))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.column = Some(id.clone());
                cx.notify();
            }))
        });
        TabBar::new("plugins-columns")
            .segmented()
            .items(items.collect::<Vec<_>>())
            .into_any_element()
    }
}
