use super::*;

/// The install popover's body: the accounts to install to, the scope, and
/// the command each account runs. `marketplace` is where the plugin browsed
/// comes from, named for an account whose own listing does not offer it.
pub(super) fn install_body(
    panel: &Entity<PluginsSettingsPanel>,
    kind: ProviderKind,
    accounts: &[Account],
    name: &str,
    marketplace: Option<&str>,
    compact: bool,
    cx: &mut App,
) -> AnyElement {
    let this = panel.read(cx);
    let Some(draft) = this.install.as_ref() else {
        return div().into_any_element();
    };
    let project = this.project(cx);
    let entry_id = draft.entry_id.clone();
    let chosen = draft.accounts.clone();
    let scope = draft.scope;
    let muted = cx.theme().muted_foreground;
    // The scopes every ticked account can install to, in the Harness's order.
    let offered: Vec<PluginScope> = plugins_of(this.store.read(cx), kind)
        .scopes
        .iter()
        .copied()
        .filter(|scope| {
            let mut ticked = accounts
                .iter()
                .filter(|account| chosen.contains(&account.id))
                .peekable();
            ticked.peek().is_some()
                && ticked.all(|account| install_scopes(account, &entry_id).contains(scope))
        })
        .collect();
    let scope = if offered.contains(&scope) {
        scope
    } else {
        offered.first().copied().unwrap_or(scope)
    };
    let project_name = project
        .as_ref()
        .map(|project| project.name.clone())
        .unwrap_or_default();
    let account_rows = accounts.iter().map(|account| {
        let scopes = install_scopes(account, &entry_id);
        let id = account.id.clone();
        let panel = panel.clone();
        let selector = format!("plugin-install-account-{}", account.id);
        let reason = scopes.is_empty().then(|| {
            account
                .user
                .as_ref()
                .and_then(|catalog| catalog.entries.iter().find(|entry| entry.id == entry_id))
                .and_then(|entry| entry.source.marketplace.clone())
                .or_else(|| marketplace.map(str::to_owned))
                .map(|marketplace| {
                    crate::tr!(
                        "providers.plugins.no_marketplace",
                        name = account.name,
                        marketplace = marketplace
                    )
                    .into_owned()
                })
                .unwrap_or_else(|| {
                    crate::tr!("providers.plugins.not_offered", name = account.name).into_owned()
                })
        });
        v_flex()
            .debug_selector(move || selector.clone())
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(material::status_dot(account.color))
                    .child(
                        Checkbox::new(SharedString::from(format!(
                            "plugin-install-account-{}",
                            account.id
                        )))
                        .label(account.name.clone())
                        .checked(chosen.contains(&account.id))
                        .disabled(scopes.is_empty())
                        .on_click(move |checked, _, cx| {
                            let checked = *checked;
                            let id = id.clone();
                            panel.update(cx, |this, cx| {
                                if let Some(draft) = this.install.as_mut() {
                                    if checked {
                                        draft.accounts.insert(id);
                                    } else {
                                        draft.accounts.remove(&id);
                                    }
                                }
                                cx.notify();
                            });
                        }),
                    ),
            )
            .children(reason.map(|reason| {
                div()
                    .pl_5()
                    .text_secondary(compact, cx)
                    .text_color(muted)
                    .child(reason)
            }))
    });
    let scope_label = |scope: PluginScope| match scope {
        PluginScope::Project => crate::tr!(
            "providers.plugins.install_scope.project",
            project = project_name
        )
        .into_owned(),
        PluginScope::Local => crate::tr!(
            "providers.plugins.install_scope.local",
            project = project_name
        )
        .into_owned(),
        _ => crate::tr!("providers.plugins.install_scope.user").into_owned(),
    };
    let scope_help = match scope {
        PluginScope::Project => crate::tr!("providers.plugins.install_scope.project_help"),
        PluginScope::Local => crate::tr!("providers.plugins.install_scope.local_help"),
        _ => crate::tr!("providers.plugins.install_scope.user_help"),
    };
    let scopes = (offered.len() > 1).then(|| {
        let choices = offered.iter().map(|option| {
            let option = *option;
            let panel = panel.clone();
            Choice::new(
                SharedString::from(format!("plugin-install-scope-{}", scope_key(option))),
                scope_label(option),
            )
            .selected(option == scope)
            .on_select(move |_, cx| {
                panel.update(cx, |this, cx| {
                    if let Some(draft) = this.install.as_mut() {
                        draft.scope = option;
                    }
                    cx.notify();
                });
            })
        });
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_caption(cx)
                    .child(crate::tr!("providers.plugins.scope_title")),
            )
            .child(
                ChoiceGroup::new("plugin-install-scopes")
                    .compact(compact)
                    .choices(choices.collect::<Vec<_>>()),
            )
            .child(
                div()
                    .text_secondary(compact, cx)
                    .text_color(muted)
                    .child(scope_help.into_owned()),
            )
    });
    let commands: Vec<AnyElement> = accounts
        .iter()
        .filter(|account| chosen.contains(&account.id))
        .filter_map(|account| {
            let op = PluginOp::Install {
                id: entry_id.clone(),
                scope,
                accept_command: None,
            };
            let line = agent::plugin_op_command(kind, account.binary.as_deref(), &op)?;
            let selector = format!("plugin-install-command-{}", account.id);
            Some(
                v_flex()
                    .debug_selector(move || selector.clone())
                    .gap_0p5()
                    .child(div().text_code(cx).child(verbatim(line, cx)))
                    .child(div().text_secondary(compact, cx).text_color(muted).child(
                        crate::tr!("providers.plugins.runs_as", name = account.name).into_owned(),
                    ))
                    .into_any_element(),
            )
        })
        .collect();
    let count = chosen.len();
    let confirm_panel = panel.clone();
    let cancel_panel = panel.clone();
    v_flex()
        .id("plugin-install-body")
        .map(|col| {
            if compact {
                col.w_full()
            } else {
                col.w(px(380.))
            }
        })
        .p_3()
        .gap_3()
        .when(!compact, |col| {
            col.child(
                div()
                    .text_body(false, cx)
                    .font_medium()
                    .child(crate::tr!("providers.plugins.install_title", name = name).into_owned()),
            )
        })
        .child(
            v_flex()
                .gap_1p5()
                .child(
                    div()
                        .text_caption(cx)
                        .child(crate::tr!("providers.plugins.install_accounts")),
                )
                .children(account_rows),
        )
        .children(scopes)
        .when(!commands.is_empty(), |col| {
            col.child(material::card(cx).w_full().gap_2().children(commands))
        })
        .child(
            h_flex()
                .w_full()
                .justify_end()
                .gap_2()
                .when(!compact, |row| {
                    row.child(
                        Button::new("plugin-install-cancel")
                            .ghost()
                            .label(crate::tr!("settings.cancel").into_owned())
                            .on_click(move |_, _, cx| {
                                cancel_panel.update(cx, |this, cx| {
                                    this.install = None;
                                    cx.notify();
                                })
                            }),
                    )
                })
                .child(
                    Button::new("plugin-install-confirm")
                        .debug_selector(|| "plugin-install-confirm".into())
                        .primary()
                        .when(compact, |button| button.w_full())
                        .disabled(count == 0)
                        .label(if count > 1 {
                            crate::tr!("providers.plugins.install_to_count", count = count)
                                .into_owned()
                        } else {
                            crate::tr!("providers.plugins.action.install").into_owned()
                        })
                        .on_click(move |_, _, cx| {
                            confirm_panel.update(cx, |this, cx| {
                                if let Some(draft) = this.install.as_mut() {
                                    draft.scope = scope;
                                }
                                this.confirm_install(cx)
                            })
                        }),
                ),
        )
        .into_any_element()
}

pub(super) fn note_row(text: String, cx: &App) -> AnyElement {
    material::group_row_stacked(cx)
        .justify_center()
        .text_secondary(false, cx)
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

/// A card holding one muted line, for a section with nothing to list.
pub(super) fn stopped(catalog: &ProviderPluginCatalog) -> bool {
    catalog.state
        == PluginCatalogState::Stale {
            reason: PluginStaleReason::Stopped,
        }
}

pub(super) fn note_card(text: String, cx: &App) -> AnyElement {
    material::grouped(vec![note_row(text, cx)], cx).into_any_element()
}

pub(super) fn source_chip(entry: &ProviderPluginEntry, cx: &App) -> AnyElement {
    let label = match (&entry.source.marketplace, entry.source.kind) {
        (Some(marketplace), PluginSourceKind::Unknown) => marketplace.clone(),
        (Some(marketplace), kind) => format!("{marketplace} · {}", source_kind_label(kind)),
        (None, kind) => source_kind_label(kind),
    };
    material::semantic_chip(label, cx.theme().muted, cx.theme().muted_foreground, cx)
        .into_any_element()
}

/// The counts of what a plugin declares, and what it adds to every session.
pub(super) fn components_line(
    entry: &ProviderPluginEntry,
    compact: bool,
    cx: &App,
) -> Option<AnyElement> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(declared) = &entry.declared {
        for (key, names) in [
            ("skills", &declared.skills),
            ("agents", &declared.agents),
            ("hooks", &declared.hooks),
            ("mcp_servers", &declared.mcp_servers),
            ("lsp_servers", &declared.lsp_servers),
            ("apps", &declared.apps),
        ] {
            if let Some(names) = names.as_ref().filter(|names| !names.is_empty()) {
                parts.push(format!(
                    "{} {}",
                    crate::tr!(format!("providers.plugins.declared.{key}")),
                    names.len()
                ));
            }
        }
        if let Some(untrusted) = declared
            .untrusted_hooks
            .as_ref()
            .filter(|hooks| !hooks.is_empty())
        {
            parts.push(format!(
                "{} {}",
                crate::tr!("providers.plugins.declared.untrusted_hooks"),
                untrusted.len()
            ));
        }
    }
    if let Some(cost) = &entry.always_on {
        parts.push(crate::tr!("providers.plugins.always_on", cost = cost).into_owned());
    }
    (!parts.is_empty()).then(|| {
        div()
            .text_secondary(compact, cx)
            .text_color(cx.theme().muted_foreground)
            .child(parts.join(" · "))
            .into_any_element()
    })
}

/// A plugin's details: what it declares by name, and each account's copy.
pub(super) fn details(
    columns: &[Account],
    row: &Row,
    project: bool,
    scope: PluginScope,
    cx: &App,
) -> AnyElement {
    let entry = &row.entry;
    let muted = cx.theme().muted_foreground;
    let mut col = v_flex().w_full().pt_2().gap_2();
    if let Some(declared) = &entry.declared {
        for (key, names) in [
            ("skills", &declared.skills),
            ("agents", &declared.agents),
            ("hooks", &declared.hooks),
            ("untrusted_hooks", &declared.untrusted_hooks),
            ("mcp_servers", &declared.mcp_servers),
            ("lsp_servers", &declared.lsp_servers),
            ("apps", &declared.apps),
        ] {
            let Some(names) = names else {
                continue;
            };
            col = col.child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2()
                    .text_secondary(false, cx)
                    .child(div().w(px(96.)).flex_none().text_color(muted).child(
                        crate::widgets::fade_text(
                            crate::tr!(format!("providers.plugins.declared.{key}")).into_owned(),
                        ),
                    ))
                    .child(div().flex_1().min_w_0().child(if names.is_empty() {
                        crate::tr!("providers.plugins.none_declared").into_owned()
                    } else {
                        names.join(", ")
                    })),
            );
        }
    }
    for (account, cell) in columns.iter().zip(&row.cells) {
        let Some(cell) = cell else {
            continue;
        };
        let scopes: Vec<PluginScope> = if project {
            vec![PluginScope::Project, PluginScope::Local]
        } else {
            vec![scope]
        };
        for scope in scopes {
            let Some(installation) = installation(cell, scope) else {
                continue;
            };
            col = col.child(
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .text_secondary(false, cx)
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(material::status_dot(account.color))
                            .child(crate::widgets::fade_text(account.name.clone()))
                            .child(
                                div()
                                    .text_color(muted)
                                    .child(crate::widgets::fade_text(scope_label(scope))),
                            )
                            .children(
                                installation
                                    .version
                                    .clone()
                                    .map(|version| div().text_meta_mono(false, cx).child(version)),
                            ),
                    )
                    .children(installation.location.as_ref().map(|location| {
                        div()
                            .pl_3()
                            .text_meta_mono(false, cx)
                            .child(location.display().to_string())
                    })),
            );
        }
        for (key, text) in &cell.diagnostics {
            col = col.child(
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .child(div().text_caption(cx).child(format!(
                        "{} · {}",
                        diagnostic_label(key),
                        account.name
                    )))
                    .child(
                        material::card(cx)
                            .w_full()
                            .text_code(cx)
                            .child(verbatim(text.clone(), cx)),
                    ),
            );
        }
    }
    col.into_any_element()
}

pub(super) fn challenge_body(challenge: &PluginChallenge, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let field = |label: SharedString, value: String| {
        v_flex()
            .w_full()
            .gap_0p5()
            .child(div().text_caption(cx).child(label))
            .child(div().w_full().text_code(cx).child(verbatim(value, cx)))
    };
    let body = match &challenge.kind {
        PluginChallengeKind::AcceptCommand {
            command,
            sha256,
            mode,
        } => v_flex()
            .w_full()
            .gap_3()
            .when_some(challenge.native_text.clone(), |col, text| {
                col.child(
                    material::card(cx)
                        .w_full()
                        .text_code(cx)
                        .child(verbatim(text, cx)),
                )
            })
            .child(field(
                crate::tr!("providers.plugins.challenge.command")
                    .into_owned()
                    .into(),
                command.clone(),
            ))
            .child(field(
                crate::tr!("providers.plugins.challenge.sha256")
                    .into_owned()
                    .into(),
                sha256.clone(),
            ))
            .when_some(mode.clone(), |col, mode| {
                col.child(field(
                    crate::tr!("providers.plugins.challenge.mode")
                        .into_owned()
                        .into(),
                    mode,
                ))
            }),
        PluginChallengeKind::ConfirmDestructive { uninstalls } => v_flex()
            .w_full()
            .gap_2()
            .text_body(false, cx)
            .child(if uninstalls.is_empty() {
                crate::tr!("providers.plugins.challenge.destructive_none")
            } else {
                crate::tr!("providers.plugins.challenge.destructive_body")
            })
            .children(uninstalls.iter().map(|id| {
                let selector = format!("plugin-challenge-uninstall-{id}");
                div()
                    .debug_selector(move || selector.clone())
                    .pl_3()
                    .child(verbatim(id.clone(), cx))
            }))
            .when_some(challenge.native_text.clone(), |col, text| {
                col.child(div().text_color(muted).child(verbatim(text, cx)))
            }),
    };
    div()
        .id("plugin-challenge-body")
        .w_full()
        // An alert's description does not shrink with the window, so the body
        // caps itself to keep the alert's actions on screen.
        .max_h(px(360.))
        .overflow_y_scroll_area()
        .child(body)
        .into_any_element()
}

/// Native text exactly as the CLI printed it, in the monospace face.
pub(super) fn verbatim(text: String, cx: &App) -> AnyElement {
    div()
        .w_full()
        .font_family(cx.theme().mono_font_family.clone())
        .child(text)
        .into_any_element()
}

pub(super) fn scope_key(scope: PluginScope) -> &'static str {
    match scope {
        PluginScope::User => "user",
        PluginScope::Project => "project",
        PluginScope::Local => "local",
        PluginScope::Managed => "managed",
        PluginScope::Session => "session",
    }
}

pub(super) fn scope_label(scope: PluginScope) -> String {
    crate::tr!(format!("providers.plugins.scope.{}", scope_key(scope))).into_owned()
}

pub(super) fn source_kind_label(kind: PluginSourceKind) -> String {
    match kind {
        PluginSourceKind::Official => crate::tr!("providers.plugins.source.official"),
        PluginSourceKind::ThirdParty => crate::tr!("providers.plugins.source.third_party"),
        PluginSourceKind::LocalPath => crate::tr!("providers.plugins.source.local_path"),
        PluginSourceKind::Git => crate::tr!("providers.plugins.source.git"),
        PluginSourceKind::Npm => crate::tr!("providers.plugins.source.npm"),
        PluginSourceKind::Command => crate::tr!("providers.plugins.source.command"),
        PluginSourceKind::Unknown => crate::tr!("providers.plugins.source.unknown"),
    }
    .into_owned()
}

/// Diagnostics are keyed by stable identifiers; one this client does not
/// know yet is shown as its key.
pub(super) fn diagnostic_label(key: &str) -> String {
    match key {
        "details" | "details_error" | "message" | "update_outcome" | "old_version"
        | "new_version" => crate::tr!(format!("providers.plugins.diagnostic.{key}")).into_owned(),
        other => other.to_string(),
    }
}

/// What the host says Settings → Plugins does with `kind`'s plugins: nothing
/// before its description arrives.
pub(super) fn plugins_of(
    store: &WorkspaceStore,
    kind: ProviderKind,
) -> tcode_protocol::PluginsDescription {
    crate::harness_dialog::describe(store, &crate::harness_dialog::HarnessId::Native(kind))
        .map(|description| description.plugins)
        .unwrap_or_default()
}

/// Whether the host says each of `kind`'s accounts keeps its plugins in its
/// own home.
pub(super) fn home_path_of(store: &WorkspaceStore, kind: ProviderKind) -> bool {
    crate::harness_dialog::describe(store, &crate::harness_dialog::HarnessId::Native(kind))
        .is_some_and(|description| description.home_path)
}
