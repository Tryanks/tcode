use super::*;

impl PluginsSettingsPanel {
    /// A notice for each account whose listing failed, with its native text.
    pub(super) fn failures(
        &self,
        accounts: &[Account],
        kind: ProviderKind,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        for account in accounts {
            let errors: Vec<String> = account
                .failure()
                .into_iter()
                .chain(
                    [&account.user, &account.project]
                        .into_iter()
                        .flatten()
                        .flat_map(|catalog| catalog.errors.iter().cloned()),
                )
                .collect();
            let failed = account.failure().is_some();
            let stopped = !failed && account.stopped();
            if errors.is_empty() && !stopped {
                continue;
            }
            let tone = if failed {
                cx.theme().danger
            } else {
                cx.theme().warning
            };
            let id = format!("plugins-failure-{}", account.id);
            notices.push(
                material::notice(tone, cx)
                    .debug_selector(move || id.clone())
                    .child(
                        h_flex()
                            .w_full()
                            .items_start()
                            .gap_3()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(div().text_body(false, cx).child(if failed {
                                        crate::tr!(
                                            "providers.plugins.list_failed",
                                            name = account.name
                                        )
                                        .into_owned()
                                    } else if stopped {
                                        crate::tr!(
                                            "providers.plugins.list_stopped",
                                            name = account.name
                                        )
                                        .into_owned()
                                    } else {
                                        crate::tr!(
                                            "providers.plugins.catalog_errors",
                                            name = account.name
                                        )
                                        .into_owned()
                                    }))
                                    .children(errors.into_iter().map(|error| {
                                        div().text_meta_mono(false, cx).child(verbatim(error, cx))
                                    })),
                            )
                            .when(failed || stopped, |row| {
                                row.child(
                                    Button::new(SharedString::from(format!(
                                        "plugins-retry-{}",
                                        account.id
                                    )))
                                    .outline()
                                    .small()
                                    .label(crate::tr!("providers.plugins.retry").into_owned())
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.relist(kind, cx)),
                                    ),
                                )
                            }),
                    )
                    .into_any_element(),
            );
        }
        notices
    }

    /// The way into installing: a harness without a catalog takes a package
    /// source, any other browses its catalog.
    pub(super) fn install_button(
        &self,
        id: &'static str,
        kind: ProviderKind,
        accounts: &[Account],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Button {
        let management = plugins_of(self.store.read(cx), kind);
        let by_source = management.source_install;
        let accounts = accounts.to_vec();
        let project = management
            .scopes
            .contains(&PluginScope::Project)
            .then(|| self.project_root(cx))
            .flatten();
        Button::new(id)
            .primary()
            .when(!compact, |button| button.small())
            .when(compact, |button| button.w_full())
            .icon(if by_source {
                IconName::Plus
            } else {
                IconName::Search
            })
            .label(if by_source {
                crate::tr!("providers.plugins.add_package").into_owned()
            } else {
                crate::tr!("providers.plugins.browse").into_owned()
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if by_source {
                    this.open_add_source(
                        AddedSource::Package,
                        accounts.clone(),
                        project.clone(),
                        window,
                        cx,
                    );
                } else {
                    this.open(Page::Browse, kind, cx);
                }
            }))
    }

    pub(super) fn empty_user(
        &self,
        kind: ProviderKind,
        accounts: &[Account],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let installs = plugins_of(self.store.read(cx), kind).supports(PluginActionKind::Install);
        material::grouped(
            vec![
                material::group_row_stacked(cx)
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_secondary(self.touch, cx)
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                crate::tr!(
                                    "providers.plugins.no_installed",
                                    name = kind.display_name()
                                )
                                .into_owned(),
                            ),
                    )
                    .when(installs, |col| {
                        col.child(self.install_button(
                            "plugins-empty-browse",
                            kind,
                            accounts,
                            false,
                            cx,
                        ))
                    })
                    .into_any_element(),
            ],
            cx,
        )
        .into_any_element()
    }

    /// A captioned group; wide, the caption carries a heading per column.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn section(
        &self,
        caption: String,
        lead: Option<AnyElement>,
        columns: &[Account],
        compact: bool,
        body: AnyElement,
        note: Option<String>,
        cx: &App,
    ) -> AnyElement {
        let headings = (!compact).then(|| {
            h_flex()
                .flex_none()
                .children(columns.iter().map(|account| {
                    h_flex()
                        .w(px(CELL_WIDTH))
                        .justify_center()
                        .gap_1()
                        .overflow_hidden()
                        .child(material::status_dot(account.color))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_caption(cx)
                                .child(crate::widgets::fade_text(account.name.clone())),
                        )
                }))
                .child(div().w(px(MENU_WIDTH)))
        });
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .pl(px(material::CARD_INSET))
                    .pr(px(material::CARD_INSET))
                    .pb(cx.theme().tokens.spacing.sm)
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .text_caption(cx)
                            .child(caption)
                            .children(lead),
                    )
                    .children(headings),
            )
            .child(body)
            .children(note.map(|note| {
                div()
                    .pt_2()
                    .pl(px(material::CARD_INSET))
                    .text_secondary(self.touch, cx)
                    .text_color(cx.theme().muted_foreground)
                    .child(note)
            }))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn plugin_rows(
        &mut self,
        columns: &[Account],
        rows: Vec<Row>,
        project: bool,
        scope: PluginScope,
        kind: ProviderKind,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Rows are keyed by plugin, so one installed or removed folds in or
        // out where it is and the rest glide to their places.
        let id = format!("plugin-rows-{}", scope_key(scope));
        let rows = rows
            .into_iter()
            .map(|row| (SharedString::from(row.entry.id.clone()), Rc::new(row)));
        if compact {
            let cards = crate::motion::keyed(
                SharedString::from(id),
                rows,
                ROWS,
                window,
                cx,
                |_, row, window, cx| {
                    div()
                        .w_full()
                        .pb_3()
                        .child(self.plugin_card(columns, row, project, scope, kind, window, cx))
                        .into_any_element()
                },
            );
            return cards.render(v_flex().w_full()).into_any_element();
        }
        let rows = crate::settings_rows::grouped_keyed(
            SharedString::from(id),
            rows,
            window,
            cx,
            |_, row, window, cx| self.plugin_row(columns, row, project, scope, kind, window, cx),
        );
        material::group(cx).child(rows).into_any_element()
    }

    /// What describes a plugin, the same in every account.
    pub(super) fn plugin_summary(
        &self,
        row: &Row,
        scope: PluginScope,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entry = &row.entry;
        let expanded = self.expanded.contains(&entry.id);
        let id = entry.id.clone();
        let toggle_id = format!("plugin-details-{}-{}", scope_key(scope), entry.id);
        let muted = cx.theme().muted_foreground;
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .child(
                h_flex()
                    .id(SharedString::from(toggle_id.clone()))
                    .debug_selector(move || toggle_id.clone())
                    .w_full()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded.remove(&id) {
                            this.expanded.insert(id.clone());
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_body(self.touch, cx)
                            .font_medium()
                            .child(crate::widgets::fade_text(entry.name.clone())),
                    )
                    .child(source_chip(entry, cx))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .text_color(muted),
                    ),
            )
            .when_some(entry.description.clone(), |col, description| {
                col.child(div().text_secondary(self.touch, cx).child(description))
            })
            .children(components_line(entry, self.touch, cx))
            .children(entry.errors.iter().map(|error| {
                div()
                    .text_secondary(self.touch, cx)
                    .text_color(cx.theme().danger)
                    .child(verbatim(error.clone(), cx))
            }))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn plugin_row(
        &self,
        columns: &[Account],
        row: &Row,
        project: bool,
        scope: PluginScope,
        kind: ProviderKind,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row_id = format!("plugin-row-{}-{}", scope_key(scope), row.entry.id);
        let differ = row.versions_differ(scope);
        let right: AnyElement = if project {
            self.project_switches(columns, row, cx)
        } else {
            h_flex()
                .flex_none()
                .items_start()
                .children(columns.iter().zip(&row.cells).map(|(account, cell)| {
                    div()
                        .w(px(CELL_WIDTH))
                        .flex()
                        .justify_center()
                        .child(self.cell(
                            account,
                            cell.as_ref(),
                            &row.entry,
                            scope,
                            differ,
                            false,
                            cx,
                        ))
                }))
                .into_any_element()
        };
        let expanded = self.expanded.contains(&row.entry.id);
        let summary = self.plugin_summary(row, scope, cx);
        let readiness = project.then(|| self.readiness(columns, row, kind, cx));
        material::group_row_stacked(cx)
            .id(SharedString::from(row_id.clone()))
            .debug_selector(move || row_id.clone())
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_3()
                    .child(
                        Icon::new(IconName::Puzzle)
                            .text_color(cx.theme().muted_foreground)
                            .mt_0p5(),
                    )
                    .child(summary)
                    .child(right)
                    .child(self.row_menu(columns, row, project, scope, cx)),
            )
            .children(readiness)
            .child(
                Collapsible::new(
                    SharedString::from(format!("plugin-more-{}", row.entry.id)),
                    expanded,
                )
                .motion(&self.motion)
                .w_full()
                .content(details(columns, row, project, scope, cx)),
            )
            .into_any_element()
    }

    /// A phone's card for one plugin: what describes it, then a row per
    /// account, or one row for the project every account shares.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plugin_card(
        &self,
        columns: &[Account],
        row: &Row,
        project: bool,
        scope: PluginScope,
        kind: ProviderKind,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row_id = format!("plugin-row-{}-{}", scope_key(scope), row.entry.id);
        let differ = row.versions_differ(scope);
        let expanded = self.expanded.contains(&row.entry.id);
        let mut lines = vec![
            material::group_row_stacked(cx)
                .gap_1()
                .child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap_2()
                        .child(self.plugin_summary(row, scope, cx))
                        .child(self.row_menu(columns, row, project, scope, cx)),
                )
                .child(
                    Collapsible::new(
                        SharedString::from(format!("plugin-more-{}", row.entry.id)),
                        expanded,
                    )
                    .motion(&self.motion)
                    .w_full()
                    .content(details(columns, row, project, scope, cx)),
                )
                .into_any_element(),
        ];
        if project {
            lines.push(
                material::group_row(cx)
                    .child(div().flex_1().min_w_0().text_body(self.touch, cx).child(
                        crate::widgets::fade_text(
                            crate::tr!("providers.plugins.this_project_shared").into_owned(),
                        ),
                    ))
                    .child(self.project_switches(columns, row, cx))
                    .into_any_element(),
            );
            lines.push(
                material::group_row_stacked(cx)
                    .child(self.readiness(columns, row, kind, cx))
                    .into_any_element(),
            );
        } else {
            for (account, cell) in columns.iter().zip(&row.cells) {
                lines.push(
                    material::group_row(cx)
                        .when(self.touch, |row| row.min_h(px(material::TOUCH_TARGET)))
                        .gap_2()
                        .child(material::status_dot(account.color))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_body(self.touch, cx)
                                .child(crate::widgets::fade_text(account.name.clone())),
                        )
                        .child(self.cell(
                            account,
                            cell.as_ref(),
                            &row.entry,
                            scope,
                            differ,
                            true,
                            cx,
                        ))
                        .into_any_element(),
                );
            }
        }
        div()
            .id(SharedString::from(row_id.clone()))
            .debug_selector(move || row_id.clone())
            .w_full()
            .child(material::grouped(lines, cx))
            .into_any_element()
    }

    /// One account's state for a plugin in its own scope.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn cell(
        &self,
        account: &Account,
        entry: Option<&ProviderPluginEntry>,
        described: &ProviderPluginEntry,
        scope: PluginScope,
        differ: bool,
        inline: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let cell_id = format!("plugin-cell-{}-{}", account.id, described.id);
        let busy = account.busy();
        let lines = if inline {
            h_flex().items_center().gap_2()
        } else {
            v_flex().items_center().gap_0p5()
        };
        let lines = lines.debug_selector({
            let cell_id = cell_id.clone();
            move || cell_id.clone()
        });
        let Some(catalog) = account.user.as_ref() else {
            return lines
                .child(Spinner::new().small().color(muted))
                .into_any_element();
        };
        let Some(entry) = entry else {
            if catalog.loading && catalog.entries.is_empty() {
                return lines
                    .child(Spinner::new().small().color(muted))
                    .into_any_element();
            }
            let reason = match described.source.marketplace.as_deref() {
                Some(marketplace)
                    if !catalog
                        .marketplaces
                        .iter()
                        .any(|known| known.name == marketplace) =>
                {
                    crate::tr!(
                        "providers.plugins.no_marketplace",
                        name = account.name,
                        marketplace = marketplace
                    )
                }
                _ => crate::tr!("providers.plugins.not_offered", name = account.name),
            };
            return lines
                .child(
                    div()
                        .id(SharedString::from(format!("{cell_id}-none")))
                        .text_color(muted)
                        .child("—")
                        .with_tooltip({
                            let reason = reason.into_owned();
                            move |_, _| Tooltip::new(reason.clone())
                        }),
                )
                .into_any_element();
        };
        let installed = installation(entry, scope);
        let pending = account.pending(&entry.id);
        let enable = PluginAction::Enable { scope };
        let disable = PluginAction::Disable { scope };
        let toggles = offers(entry, enable) || offers(entry, disable);
        let control: AnyElement = if pending {
            Spinner::new().small().color(muted).into_any_element()
        } else if installed.is_some() && toggles {
            let on = scope_enabled(entry, scope);
            let (account_id, entry_id) = (account.id.clone(), entry.id.clone());
            let action = if on { disable } else { enable };
            let switch_id = format!("plugin-switch-{}-{}", account.id, entry.id);
            div()
                .debug_selector(move || switch_id.clone())
                .child(
                    Switch::new(SharedString::from(format!(
                        "plugin-switch-{}-{}",
                        account.id, entry.id
                    )))
                    .checked(on)
                    .disabled(busy || !offers(entry, action))
                    .tooltip(account.name.clone())
                    .on_click(cx.listener(
                        move |this, checked: &bool, _, cx| {
                            let action = if *checked { enable } else { disable };
                            this.run(&account_id, &entry_id, action, None, cx);
                        },
                    )),
                )
                .into_any_element()
        } else if installed.is_some() {
            material::semantic_chip(
                crate::tr!("providers.plugins.installed_state").into_owned(),
                cx.theme().muted,
                muted,
                cx,
            )
            .into_any_element()
        } else if offers(entry, PluginAction::Install { scope }) {
            let (account_id, entry_id) = (account.id.clone(), entry.id.clone());
            let id = format!("plugin-install-{}-{}", account.id, entry.id);
            Button::new(SharedString::from(id.clone()))
                .debug_selector(move || id.clone())
                .outline()
                .small()
                .disabled(busy)
                .label(crate::tr!("providers.plugins.action.install").into_owned())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run(
                        &account_id,
                        &entry_id,
                        PluginAction::Install { scope },
                        None,
                        cx,
                    )
                }))
                .into_any_element()
        } else {
            div().text_color(muted).child("—").into_any_element()
        };
        let version = installed.and_then(|installation| installation.version.clone());
        let fact: Option<AnyElement> = match (&entry.latest_version, installed) {
            (Some(latest), Some(_)) => Some(
                div()
                    .text_token(&cx.theme().tokens.typography.xs)
                    .text_color(cx.theme().primary)
                    .child(crate::widgets::fade_text(
                        crate::tr!("providers.plugins.latest", version = latest).into_owned(),
                    ))
                    .into_any_element(),
            ),
            (None, Some(_)) if differ => Some(
                div()
                    .text_token(&cx.theme().tokens.typography.xs)
                    .text_color(cx.theme().warning)
                    .child(crate::widgets::fade_text(
                        crate::tr!("providers.plugins.versions_differ").into_owned(),
                    ))
                    .into_any_element(),
            ),
            (_, None) if !inline => Some(
                div()
                    .text_token(&cx.theme().tokens.typography.xs)
                    .text_color(muted)
                    .child(crate::widgets::fade_text(
                        crate::tr!("providers.plugins.not_installed").into_owned(),
                    ))
                    .into_any_element(),
            ),
            _ => None,
        };
        let update = PluginAction::Update { scope };
        let update_button = (entry.latest_version.is_some() && offers(entry, update) && !pending)
            .then(|| {
                let (account_id, entry_id) = (account.id.clone(), entry.id.clone());
                Button::new(SharedString::from(format!(
                    "plugin-update-{}-{}",
                    account.id, entry.id
                )))
                .outline()
                .xsmall()
                .disabled(busy)
                .label(crate::tr!("providers.plugins.action.update").into_owned())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run(&account_id, &entry_id, update, None, cx)
                }))
            });
        let version = version.map(|version| div().text_meta_mono(inline, cx).child(version));
        if inline {
            lines
                .children(fact)
                .children(version)
                .children(update_button)
                .child(control)
                .into_any_element()
        } else {
            lines
                .child(control)
                .children(version)
                .children(fact)
                .children(update_button)
                .into_any_element()
        }
    }
}
