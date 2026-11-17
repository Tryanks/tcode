use super::*;

impl PluginsSettingsPanel {
    /// The project's own switch for each project scope the plugin is in,
    /// changed through any account that offers it: the file is shared.
    pub(super) fn project_switches(
        &self,
        columns: &[Account],
        row: &Row,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = self.project_root(cx);
        let muted = cx.theme().muted_foreground;
        let scopes = [PluginScope::Project, PluginScope::Local];
        let pending = columns.iter().any(|account| account.pending(&row.entry.id));
        let busy = columns.iter().any(Account::busy);
        let switches = scopes.into_iter().filter_map(|scope| {
            let holder = row
                .cells
                .iter()
                .flatten()
                .find(|entry| installation(entry, scope).is_some())?;
            let on = scope_enabled(holder, scope);
            let wanted = if on {
                PluginAction::Disable { scope }
            } else {
                PluginAction::Enable { scope }
            };
            let via = columns
                .iter()
                .zip(&row.cells)
                .find(|(_, cell)| cell.as_ref().is_some_and(|entry| offers(entry, wanted)))
                .map(|(account, _)| account.id.clone());
            let label = match scope {
                PluginScope::Local => crate::tr!("providers.plugins.local_only"),
                _ => crate::tr!("providers.plugins.this_project"),
            };
            let id = format!(
                "plugin-project-switch-{}-{}",
                scope_key(scope),
                row.entry.id
            );
            let entry_id = row.entry.id.clone();
            let root = root.clone();
            Some(
                h_flex()
                    .gap_2()
                    .items_center()
                    .justify_end()
                    .child(
                        div()
                            .text_secondary(false, cx)
                            .text_color(muted)
                            .child(crate::widgets::fade_text(label.into_owned())),
                    )
                    .child(
                        div().debug_selector(move || id.clone()).child(
                            Switch::new(SharedString::from(format!(
                                "plugin-project-switch-{}-{}",
                                scope_key(scope),
                                row.entry.id
                            )))
                            .checked(on)
                            .disabled(busy || via.is_none())
                            .on_click(cx.listener(
                                move |this, checked: &bool, _, cx| {
                                    let Some(via) = via.clone() else {
                                        return;
                                    };
                                    let action = if *checked {
                                        PluginAction::Enable { scope }
                                    } else {
                                        PluginAction::Disable { scope }
                                    };
                                    this.run(&via, &entry_id, action, root.clone(), cx);
                                },
                            )),
                        ),
                    ),
            )
        });
        v_flex()
            .flex_none()
            .items_end()
            .gap_1()
            .when(pending, |col| {
                col.child(Spinner::new().small().color(muted))
            })
            .children(switches)
            .into_any_element()
    }

    /// Whether each account's sessions in the project get the plugin: the
    /// enablement is shared, the files are each account's own.
    pub(super) fn readiness(
        &self,
        columns: &[Account],
        row: &Row,
        kind: ProviderKind,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = self.project_root(cx);
        let muted = cx.theme().muted_foreground;
        let items = columns.iter().zip(&row.cells).map(|(account, cell)| {
            let ready = cell.as_ref().and_then(|entry| {
                installation(entry, PluginScope::Project)
                    .or(installation(entry, PluginScope::Local))
            });
            let install = cell.as_ref().is_some_and(|entry| {
                offers(
                    entry,
                    PluginAction::Install {
                        scope: PluginScope::Project,
                    },
                )
            });
            let id = format!("plugin-ready-{}-{}", account.id, row.entry.id);
            // A short state beside the account, and for an account the
            // project's plugin does not reach, what the Harness does then.
            let (state, color, why) = match ready {
                Some(installation) => (
                    match &installation.version {
                        Some(version) => {
                            crate::tr!("providers.plugins.ready_version", version = version)
                        }
                        None => crate::tr!("providers.plugins.ready"),
                    },
                    cx.theme().success,
                    None,
                ),
                None if install => (
                    crate::tr!("providers.plugins.not_installed"),
                    cx.theme().warning,
                    Some(crate::tr!(
                        "providers.plugins.not_installed_here",
                        harness = kind.display_name()
                    )),
                ),
                None => (
                    crate::tr!("providers.plugins.cannot_install"),
                    cx.theme().danger,
                    Some(match row.entry.source.marketplace.as_deref() {
                        Some(marketplace) => crate::tr!(
                            "providers.plugins.cannot_install_here",
                            marketplace = marketplace
                        ),
                        None => crate::tr!("providers.plugins.not_offered", name = account.name),
                    }),
                ),
            };
            let (account_id, entry_id, root) =
                (account.id.clone(), row.entry.id.clone(), root.clone());
            let install_button = (ready.is_none() && install).then(|| {
                let button_id = format!("plugin-ready-install-{}-{}", account.id, row.entry.id);
                Button::new(SharedString::from(button_id.clone()))
                    .debug_selector(move || button_id.clone())
                    .ghost()
                    .xsmall()
                    .disabled(account.busy())
                    .label(
                        crate::tr!("providers.plugins.install_in", name = account.name)
                            .into_owned(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.run(
                            &account_id,
                            &entry_id,
                            PluginAction::Install {
                                scope: PluginScope::Project,
                            },
                            root.clone(),
                            cx,
                        )
                    }))
            });
            v_flex()
                .debug_selector(move || id.clone())
                .w_full()
                .gap_0p5()
                .child(
                    h_flex()
                        .w_full()
                        .flex_wrap()
                        .items_center()
                        .gap_1p5()
                        .child(material::status_dot(account.color))
                        .child(
                            div()
                                .text_color(muted)
                                .child(crate::widgets::fade_text(account.name.clone())),
                        )
                        .child(
                            div()
                                .text_color(color)
                                .child(crate::widgets::fade_text(state.into_owned())),
                        )
                        .children(install_button),
                )
                .children(why.map(|why| {
                    div()
                        .w_full()
                        .pl_3p5()
                        .text_color(muted)
                        .child(why.into_owned())
                }))
        });
        v_flex()
            .w_full()
            .gap_1()
            .text_secondary(self.touch, cx)
            .children(items)
            .into_any_element()
    }

    /// The row's menu: each account's update and uninstall the host offered.
    pub(super) fn row_menu(
        &self,
        columns: &[Account],
        row: &Row,
        project: bool,
        scope: PluginScope,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cwd = if project { self.project_root(cx) } else { None };
        let scopes: Vec<PluginScope> = if project {
            vec![PluginScope::Project, PluginScope::Local]
        } else {
            vec![scope]
        };
        let mut items: Vec<(String, RunPluginAction)> = Vec::new();
        for (account, cell) in columns.iter().zip(&row.cells) {
            let Some(entry) = cell else {
                continue;
            };
            for &scope in &scopes {
                for (action, key) in [
                    (
                        PluginAction::Update { scope },
                        if entry.latest_version.is_some() {
                            "providers.plugins.menu.update"
                        } else {
                            "providers.plugins.menu.check_update"
                        },
                    ),
                    (
                        PluginAction::Uninstall { scope },
                        "providers.plugins.menu.uninstall",
                    ),
                ] {
                    if offers(entry, action) {
                        items.push((
                            crate::tr!(key, name = account.name).into_owned(),
                            RunPluginAction {
                                profile_id: account.id.clone(),
                                entry_id: entry.id.clone(),
                                action,
                                cwd: cwd.clone(),
                            },
                        ));
                    }
                }
            }
        }
        let id = format!("plugin-menu-{}-{}", scope_key(scope), row.entry.id);
        if items.is_empty() {
            return div().w(px(MENU_WIDTH)).flex_none().into_any_element();
        }
        let busy = columns.iter().any(Account::busy);
        div()
            .w(px(MENU_WIDTH))
            .flex_none()
            .debug_selector({
                let id = id.clone();
                move || id.clone()
            })
            .child(
                Button::new(SharedString::from(id))
                    .ghost()
                    .xsmall()
                    .disabled(busy)
                    .icon(IconName::Ellipsis)
                    .tooltip(
                        crate::tr!("providers.plugins.more", name = row.entry.name).into_owned(),
                    )
                    .dropdown_menu(move |mut menu, _, _| {
                        for (label, action) in &items {
                            menu = menu.menu(label.clone(), Box::new(action.clone()));
                        }
                        menu
                    }),
            )
            .into_any_element()
    }

    pub(super) fn project_section(
        &mut self,
        kind: ProviderKind,
        columns: &[Account],
        project: Option<tcode_core::project::Project>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let projects = self.store.read(cx).projects();
        let picked = project.as_ref().map(|project| project.id.clone());
        let label = project
            .as_ref()
            .map(|project| project.name.clone())
            .unwrap_or_else(|| crate::tr!("providers.plugins.pick_project").into_owned());
        let picker = Button::new("plugins-project-picker")
            .debug_selector(|| "plugins-project-picker".into())
            .outline()
            .xsmall()
            .icon(IconName::Folder)
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                for project in &projects {
                    menu = menu.menu_with_check(
                        project.name.clone(),
                        picked.as_ref() == Some(&project.id),
                        Box::new(ChooseProject {
                            project_id: project.id.clone(),
                        }),
                    );
                }
                menu
            });
        let lead = h_flex()
            .items_center()
            .gap_2()
            .child(picker)
            .child(crate::widgets::fade_text(
                crate::tr!("providers.plugins.project_shared").into_owned(),
            ))
            .into_any_element();
        let body = match &project {
            None => note_card(
                crate::tr!("providers.plugins.project_needed").into_owned(),
                cx,
            ),
            Some(_) => {
                let project_rows = {
                    let mut rows_found = rows(columns, true, PluginScope::Project);
                    for row in rows(columns, true, PluginScope::Local) {
                        if !rows_found
                            .iter()
                            .any(|found| found.entry.id == row.entry.id)
                        {
                            rows_found.push(row);
                        }
                    }
                    rows_found
                };
                let listing = columns.iter().all(|account| {
                    account
                        .project
                        .as_ref()
                        .is_none_or(|catalog| catalog.loading && catalog.entries.is_empty())
                });
                if !project_rows.is_empty() {
                    self.plugin_rows(
                        columns,
                        project_rows,
                        true,
                        PluginScope::Project,
                        kind,
                        compact,
                        window,
                        cx,
                    )
                } else if listing {
                    note_card(crate::tr!("providers.plugins.listing").into_owned(), cx)
                } else if columns
                    .iter()
                    .any(|account| account.project.as_ref().is_some_and(stopped))
                {
                    note_card(crate::tr!("providers.plugins.stopped").into_owned(), cx)
                } else {
                    note_card(
                        crate::tr!("providers.plugins.no_project_plugins").into_owned(),
                        cx,
                    )
                }
            }
        };
        let note = (project.is_some()
            && plugins_of(self.store.read(cx), kind)
                .scopes
                .contains(&PluginScope::Local))
        .then(|| crate::tr!("providers.plugins.local_note").into_owned());
        let caption = crate::tr!("providers.plugins.project_scope").into_owned();
        // A project's plugins have one switch for every account, so the
        // caption carries no account headings.
        self.section(caption, Some(lead), &[], compact, body, note, cx)
    }
}
