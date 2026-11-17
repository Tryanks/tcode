use super::*;

impl PluginsSettingsPanel {
    pub(super) fn marketplaces(
        &self,
        columns: &[Account],
        accounts: &[Account],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut names: Vec<ProviderPluginMarketplace> = Vec::new();
        for account in columns {
            for marketplace in account
                .user
                .iter()
                .flat_map(|catalog| &catalog.marketplaces)
            {
                if !names.iter().any(|known| known.name == marketplace.name) {
                    names.push(marketplace.clone());
                }
            }
        }
        let names_empty = names.is_empty();
        // A listing still running has not said which marketplaces there are.
        let listing = names_empty
            && columns.iter().all(|account| {
                account
                    .user
                    .as_ref()
                    .is_none_or(|catalog| catalog.loading && catalog.marketplaces.is_empty())
            });
        let success = cx.theme().success;
        let muted = cx.theme().muted_foreground;
        let mut rows: Vec<AnyElement> = names
            .into_iter()
            .map(|marketplace| {
                let cells = columns.iter().map(|account| {
                    let catalog = account.user.as_ref();
                    let added = catalog.is_some_and(|catalog| {
                        catalog.marketplaces.iter().any(|known| known.name == marketplace.name)
                    });
                    let can_add = catalog.is_some_and(|catalog| {
                        catalog.marketplace_actions.contains(&MarketplaceAction::Add)
                    });
                    let pending = account.pending(&marketplace.name) || account.pending(&marketplace.source);
                    let id = format!("plugin-marketplace-{}-{}", account.id, marketplace.name);
                    let content: AnyElement = if pending {
                        Spinner::new().small().color(muted).into_any_element()
                    } else if added {
                        div()
                            .text_secondary(self.touch, cx)
                            .text_color(success)
                            .child(crate::widgets::fade_text(crate::tr!("providers.plugins.added").into_owned()))
                            .into_any_element()
                    } else if can_add {
                        let (profile_id, source) = (account.id.clone(), marketplace.source.clone());
                        let store = self.store.clone();
                        Button::new(SharedString::from(format!("{id}-add")))
                            .outline()
                            .small()
                            .disabled(account.busy())
                            .label(crate::tr!("providers.plugins.add").into_owned())
                            .on_click(move |_, _, cx| {
                                let (profile_id, source) = (profile_id.clone(), source.clone());
                                store.update(cx, |store, _| {
                                    store.add_provider_marketplace(profile_id, source, None)
                                });
                            })
                            .into_any_element()
                    } else {
                        div().text_color(muted).child("—").into_any_element()
                    };
                    let cell = div().debug_selector(move || id.clone());
                    if compact {
                        cell.w_full()
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .items_center()
                                    .child(material::status_dot(account.color))
                                    .child(div().flex_1().min_w_0().overflow_hidden().child(crate::widgets::fade_text(account.name.clone())))
                                    .child(content),
                            )
                            .into_any_element()
                    } else {
                        cell.w(px(CELL_WIDTH)).flex().justify_center().child(content).into_any_element()
                    }
                });
                let removals: Vec<(String, RemoveMarketplace)> = columns
                    .iter()
                    .filter(|account| {
                        account.user.as_ref().is_some_and(|catalog| {
                            catalog.marketplace_actions.iter().any(|action| {
                                matches!(action, MarketplaceAction::Remove { marketplace: name, .. } if *name == marketplace.name)
                            })
                        })
                    })
                    .map(|account| {
                        (
                            crate::tr!("providers.plugins.menu.remove_marketplace", name = account.name).into_owned(),
                            RemoveMarketplace {
                                profile_id: account.id.clone(),
                                name: marketplace.name.clone(),
                            },
                        )
                    })
                    .collect();
                let menu: AnyElement = if removals.is_empty() {
                    div().w(px(MENU_WIDTH)).into_any_element()
                } else {
                    div()
                        .w(px(MENU_WIDTH))
                        .flex_none()
                        .child(
                            Button::new(SharedString::from(format!("plugin-marketplace-menu-{}", marketplace.name)))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Ellipsis)
                                .dropdown_menu(move |mut menu, _, _| {
                                    for (label, action) in &removals {
                                        menu = menu.menu(label.clone(), Box::new(action.clone()));
                                    }
                                    menu
                                }),
                        )
                        .into_any_element()
                };
                let describe = v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(div().text_body(self.touch, cx).child(crate::widgets::fade_text(marketplace.name.clone())))
                            .child(material::semantic_chip(
                                source_kind_label(marketplace.kind),
                                cx.theme().muted,
                                muted,
                                cx,
                            )),
                    )
                    .child(div().text_meta_mono(self.touch, cx).child(marketplace.source.clone()));
                if compact {
                    material::group_row_stacked(cx)
                        .gap_2()
                        .child(h_flex().w_full().items_start().gap_2().child(describe).child(menu))
                        .children(cells)
                        .into_any_element()
                } else {
                    material::group_row_stacked(cx)
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(Icon::new(IconName::Puzzle).text_color(muted))
                        .child(describe)
                        .child(h_flex().flex_none().children(cells))
                        .child(menu)
                        .into_any_element()
                }
            })
            .collect();
        let can_add = accounts.iter().any(|account| {
            account.user.as_ref().is_some_and(|catalog| {
                catalog
                    .marketplace_actions
                    .contains(&MarketplaceAction::Add)
            })
        });
        if can_add {
            let accounts = accounts.to_vec();
            rows.push(
                material::group_row(cx)
                    .child(
                        Button::new("plugin-marketplace-add")
                            .debug_selector(|| "plugin-marketplace-add".into())
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .label(crate::tr!("providers.plugins.add_marketplace").into_owned())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_add_source(
                                    AddedSource::Marketplace,
                                    accounts.clone(),
                                    None,
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .into_any_element(),
            );
        }
        let body = if listing {
            note_card(crate::tr!("providers.plugins.listing").into_owned(), cx)
        } else if names_empty
            && columns
                .iter()
                .any(|account| account.user.as_ref().is_some_and(stopped))
        {
            note_card(crate::tr!("providers.plugins.stopped").into_owned(), cx)
        } else if rows.is_empty() {
            note_card(
                crate::tr!("providers.plugins.no_marketplaces").into_owned(),
                cx,
            )
        } else {
            material::grouped(rows, cx).into_any_element()
        };
        self.section(
            crate::tr!("providers.plugins.marketplaces").into_owned(),
            None,
            columns,
            compact,
            body,
            None,
            cx,
        )
    }

    pub(super) fn browse_page(
        &mut self,
        kind: ProviderKind,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accounts = self.accounts(kind, cx);
        let query = self.search.read(cx).value().trim().to_lowercase();
        let mut entries: Vec<ProviderPluginEntry> = Vec::new();
        for account in &accounts {
            for entry in account.user.iter().flat_map(|catalog| &catalog.entries) {
                if !entries.iter().any(|known| known.id == entry.id) {
                    entries.push(entry.clone());
                }
            }
        }
        let mut marketplaces: Vec<(String, usize)> = Vec::new();
        for entry in &entries {
            if let Some(marketplace) = &entry.source.marketplace {
                match marketplaces
                    .iter_mut()
                    .find(|(name, _)| name == marketplace)
                {
                    Some((_, count)) => *count += 1,
                    None => marketplaces.push((marketplace.clone(), 1)),
                }
            }
        }
        let total = entries.len();
        let filter = self
            .marketplace_filter
            .clone()
            .filter(|name| marketplaces.iter().any(|(known, _)| known == name));
        let matches: Vec<ProviderPluginEntry> = entries
            .into_iter()
            .filter(|entry| filter.is_none() || entry.source.marketplace == filter)
            .filter(|entry| {
                query.is_empty()
                    || entry.name.to_lowercase().contains(&query)
                    || entry
                        .description
                        .as_deref()
                        .is_some_and(|text| text.to_lowercase().contains(&query))
            })
            .collect();
        let chips = std::iter::once((
            None,
            crate::tr!("providers.plugins.all").into_owned(),
            total,
        ))
        .chain(
            marketplaces
                .into_iter()
                .map(|(name, count)| (Some(name.clone()), name, count)),
        )
        .map(|(name, label, count)| {
            let id = format!("plugins-filter-{}", name.as_deref().unwrap_or("all"));
            let selected = filter == name;
            TabItem::new(SharedString::from(id), label)
                .count(count)
                .selected(selected)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.marketplace_filter = name.clone();
                    cx.notify();
                }))
        })
        .collect::<Vec<_>>();
        let listing = accounts.iter().any(|account| {
            account
                .user
                .as_ref()
                .is_none_or(|catalog| catalog.loading && catalog.entries.is_empty())
        });
        let body = if matches.is_empty() {
            note_card(
                if listing && total == 0 {
                    crate::tr!("providers.plugins.listing")
                } else if total == 0 {
                    crate::tr!("providers.plugins.no_available")
                } else {
                    crate::tr!("providers.plugins.no_matches")
                }
                .into_owned(),
                cx,
            )
        } else {
            let rows: Vec<AnyElement> = matches
                .iter()
                .map(|entry| self.browse_row(kind, &accounts, entry, compact, window, cx))
                .collect();
            material::grouped(rows, cx).into_any_element()
        };
        v_flex()
            .w_full()
            .gap_3()
            .child(self.back_bar(kind.display_name().to_string(), compact, cx))
            .child(
                div().text_title(cx).child(
                    crate::tr!("providers.plugins.browse_title", name = kind.display_name())
                        .into_owned(),
                ),
            )
            .child(Input::new(&self.search).placeholder(crate::tr!("providers.plugins.search")))
            .child(TabBar::new("plugins-filters").segmented().items(chips))
            .child(body)
            .into_any_element()
    }

    pub(super) fn browse_row(
        &mut self,
        kind: ProviderKind,
        accounts: &[Account],
        entry: &ProviderPluginEntry,
        compact: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let installed_in: Vec<&Account> = accounts
            .iter()
            .filter(|account| {
                account
                    .entry(false, &entry.id)
                    .is_some_and(|entry| installation(entry, PluginScope::User).is_some())
            })
            .collect();
        let installable = accounts
            .iter()
            .any(|account| !install_scopes(account, &entry.id).is_empty());
        let pending = accounts.iter().any(|account| account.pending(&entry.id));
        let row_id = format!("plugin-available-{}", entry.id);
        let install: Option<AnyElement> = installable.then(|| {
            let open = self
                .install
                .as_ref()
                .is_some_and(|draft| draft.entry_id == entry.id);
            let trigger_id = format!("plugin-install-open-{}", entry.id);
            let entry_id = entry.id.clone();
            let accounts_for = accounts.to_vec();
            let trigger = Button::new(SharedString::from(trigger_id.clone()))
                .debug_selector(move || trigger_id.clone())
                .outline()
                .small()
                .label(crate::tr!("providers.plugins.install_more").into_owned())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.begin_install(entry_id.clone(), &accounts_for, cx)
                }));
            let panel = cx.entity();
            let accounts = accounts.to_vec();
            let popover = Popover::new(SharedString::from(format!("plugin-install-{}", entry.id)))
                .open(open)
                .anchor(gpui::Anchor::TopRight)
                .on_open_change({
                    let panel = panel.clone();
                    move |open, _, cx| {
                        if !*open {
                            panel.update(cx, |this, cx| {
                                this.install = None;
                                cx.notify();
                            });
                        }
                    }
                })
                .trigger(trigger);
            let popover = if self.touch {
                popover.bottom_sheet(
                    crate::tr!("providers.plugins.install_title", name = entry.name).into_owned(),
                )
            } else {
                popover
            };
            let (name, touch) = (entry.name.clone(), self.touch);
            let marketplace = entry.source.marketplace.clone();
            popover
                .content(move |_, _, cx| {
                    install_body(
                        &panel,
                        kind,
                        &accounts,
                        &name,
                        marketplace.as_deref(),
                        touch,
                        cx,
                    )
                })
                .into_any_element()
        });
        let badges = installed_in.iter().map(|account| {
            material::semantic_chip(
                crate::tr!("providers.plugins.installed_in", name = account.name).into_owned(),
                cx.theme().success.opacity(0.12),
                cx.theme().success,
                cx,
            )
        });
        material::group_row_stacked(cx)
            .id(SharedString::from(row_id.clone()))
            .debug_selector(move || row_id.clone())
            .flex_row()
            .items_start()
            .gap_3()
            .when(!compact, |row| {
                row.child(Icon::new(IconName::Puzzle).text_color(muted).mt_0p5())
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_body(self.touch, cx)
                                    .font_medium()
                                    .child(crate::widgets::fade_text(entry.name.clone())),
                            )
                            .child(source_chip(entry, cx))
                            .children(badges)
                            .when(pending, |row| {
                                row.child(Spinner::new().xsmall().color(muted))
                            }),
                    )
                    .when_some(entry.description.clone(), |col, description| {
                        col.child(div().text_secondary(self.touch, cx).child(description))
                    })
                    .children(components_line(entry, self.touch, cx))
                    .when(entry.source.kind == PluginSourceKind::Command, |col| {
                        col.child(
                            div()
                                .text_secondary(self.touch, cx)
                                .text_color(cx.theme().warning)
                                .child(crate::tr!("providers.plugins.command_source")),
                        )
                    }),
            )
            .children(install.map(|install| div().flex_none().child(install)))
            .into_any_element()
    }
}
