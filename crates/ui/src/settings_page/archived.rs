use super::*;

impl SettingsPage {
    /// Archived sessions: loading, a failed load, the empty state and the
    /// list cross-fade into one another.
    pub(super) fn render_archived(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let groups = self.store.read(cx).archived_groups();
        let failure = self.store.read(cx).archived_failure().map(str::to_owned);
        let slot = if !groups.is_empty() {
            "list"
        } else if failure.is_some() {
            "failed"
        } else if self.store.read(cx).archived_loading() {
            "loading"
        } else {
            "empty"
        };
        let mut groups = Some(groups).filter(|groups| !groups.is_empty());
        let body = crate::motion::cross_fade(
            "archived-body",
            slot,
            CHANGE,
            window,
            cx,
            |slot, window, cx| {
                match *slot {
                    "list" => match groups.take() {
                        Some(groups) => self
                            .render_archived_list(groups, window, cx)
                            .into_any_element(),
                        // A list leaving for the empty state has no rows left.
                        None => div().into_any_element(),
                    },
                    slot => v_flex()
                        .gap(crate::material::section_gap(cx))
                        .child(crate::material::group_caption(
                            crate::tr!("settings.archived_section"),
                            cx,
                        ))
                        .child(if slot == "loading" {
                            crate::material::empty_row(crate::tr!("settings.archived_loading"), cx)
                        } else if slot == "failed" {
                            crate::material::empty_row(
                                crate::tr!(
                                    "settings.archived_failed",
                                    error = failure.clone().unwrap_or_default()
                                ),
                                cx,
                            )
                        } else {
                            crate::material::empty_state(
                                Icon::new(Section::Archived.icon()),
                                crate::tr!("settings.archived_empty"),
                                String::new(),
                                cx,
                            )
                        })
                        .into_any_element(),
                }
            },
        );
        v_flex().child(body)
    }

    pub(super) fn render_archived_list(
        &mut self,
        groups: Vec<tcode_core::project::ProjectGroup>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let full = self.store.read(cx).may_operate();

        let flat: Vec<((SharedString, SharedString), SessionMeta)> = groups
            .into_iter()
            .flat_map(|group| {
                let project = (
                    SharedString::from(group.project.id),
                    SharedString::from(group.project.name),
                );
                group
                    .sessions
                    .into_iter()
                    .map(move |meta| (project.clone(), meta))
            })
            .collect();
        let total = flat.len();
        let (page, start, end) = page_bounds(total, self.archived_page, ARCHIVED_PER_PAGE);
        self.archived_page = page;

        let now = self.store.read(cx).host_now_secs(cx);
        // Each part below the controls carries the gap above it, so a project
        // whose last archived thread goes takes its gap with it.
        let gap = crate::material::section_gap(cx);
        let col = v_flex()
            .child(
                gpui_base::h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_secondary(self.window_state.read(cx).compact, cx)
                            .child(crate::widgets::fade_text(crate::tr!(
                                "settings.archived_range",
                                from = start + 1,
                                to = end,
                                count = total
                            ))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("archived-prev")
                            .outline()
                            .small()
                            .label(crate::tr!("settings.archived_prev"))
                            .disabled(page == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.archived_page = this.archived_page.saturating_sub(1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("archived-next")
                            .outline()
                            .small()
                            .label(crate::tr!("settings.archived_next"))
                            .disabled(end >= total)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.archived_page += 1;
                                cx.notify();
                            })),
                    )
                    .when(full, |row| {
                        row.child(
                            Button::new("archived-delete-all")
                                .danger()
                                .small()
                                .label(crate::tr!("settings.archived_delete_all"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm_delete_all_archived(window, cx);
                                })),
                        )
                    }),
            )
            .child(div().pt(gap).child(crate::material::group_caption(
                crate::tr!("settings.archived_section"),
                cx,
            )));
        // The page's rows by project, in order; a page turned is a new set of
        // rows, so the old page folds away as the new one folds in.
        let mut projects: Vec<(SharedString, (SharedString, Vec<SessionMeta>))> = Vec::new();
        for ((project_id, project_name), meta) in &flat[start..end] {
            match projects.last_mut() {
                Some((id, (_, sessions))) if id == project_id => sessions.push(meta.clone()),
                _ => projects.push((
                    project_id.clone(),
                    (project_name.clone(), vec![meta.clone()]),
                )),
            }
        }
        let compact = self.window_state.read(cx).compact;
        let projects = crate::motion::keyed(
            "archived-projects",
            projects,
            ROWS,
            window,
            cx,
            |project_id, (project_name, sessions), window, cx| {
                let rows = sessions
                    .iter()
                    .map(|meta| (SharedString::from(meta.id.clone()), meta.clone()));
                let rows = crate::motion::keyed(
                    SharedString::from(format!("archived-rows-{project_id}")),
                    rows,
                    ROWS,
                    window,
                    cx,
                    |_, meta, _, cx| self.archived_row(meta, now, full, compact, cx),
                );
                v_flex()
                    .pt(gap)
                    .child(crate::material::group_caption(project_name.clone(), cx))
                    .child(rows.render(crate::material::group(cx)))
                    .into_any_element()
            },
        );
        col.child(projects.render(v_flex().w_full()))
    }

    pub(super) fn archived_row(
        &self,
        meta: &SessionMeta,
        now: u64,
        full: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let created = crate::tr!(
            "settings.archived_created",
            when = humanize_ago(now.saturating_sub(meta.created_at))
        );
        let desc = match meta.archived_at {
            Some(archived_at) => format!(
                "{} · {created}",
                crate::tr!(
                    "settings.archived_at",
                    when = humanize_ago(now.saturating_sub(archived_at))
                ),
            ),
            None => created.into_owned(),
        };
        let id_unarchive = meta.id.clone();
        let id_delete = meta.id.clone();
        let title = meta.title.clone();
        let row =
            crate::settings_rows::flip_row(&self.motion, format!("archived-{}", meta.id), compact);
        let layout = row.row(
            crate::settings_rows::row_labels(
                meta.title.clone(),
                Some(desc.into()),
                None,
                compact,
                cx,
            ),
            gpui_base::h_flex()
                .flex_none()
                .gap_2()
                // Nothing is restored on a machine that lets this device only view it.
                .when(!self.store.read(cx).view_only(), |row| {
                    row.child(
                        Button::new(SharedString::from(format!("unarchive-{}", meta.id)))
                            .outline()
                            .small()
                            .label(crate::tr!("settings.unarchive"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = id_unarchive.clone();
                                this.store.update(cx, |store, _cx| {
                                    store.unarchive_session(id);
                                });
                            })),
                    )
                })
                .when(full, |row| {
                    row.child(
                        Button::new(SharedString::from(format!("delete-perm-{}", meta.id)))
                            .danger()
                            .small()
                            .label(crate::tr!("settings.delete_permanently"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete_archived(&id_delete, &title, window, cx);
                            })),
                    )
                }),
            cx,
        );
        row.render(layout)
    }

    pub(super) fn confirm_delete_archived(
        &self,
        session_id: &str,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::sidebar::confirm_delete(
            self.store.clone(),
            session_id.to_string(),
            title.to_string(),
            crate::tr!("settings.delete_permanently").into(),
            window,
            cx,
        );
    }

    /// Bulk permanent delete. Always confirms, whatever
    /// `skip_delete_confirmation` says — this destroys every archived thread.
    pub(super) fn confirm_delete_all_archived(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(deletion) = self.store.read(cx).archived_deletion() else {
            return;
        };
        let (title, description) = if deletion.unarchived == 0 {
            (
                crate::tr!(
                    "settings.archived_delete_all_title",
                    count = deletion.archived
                ),
                crate::tr!("settings.archived_delete_all_description"),
            )
        } else {
            (
                crate::tr!(
                    "settings.archived_delete_all_tree_title",
                    count = deletion.archived,
                    unarchived = deletion.unarchived
                ),
                crate::tr!(
                    "settings.archived_delete_all_tree_description",
                    total = deletion.archived + deletion.unarchived
                ),
            )
        };
        let store = self.store.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let deletion = deletion.clone();
            alert
                .title(title.clone())
                .description(description.clone())
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("settings.delete_permanently"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let deletion = deletion.clone();
                    store.update(cx, |store, _cx| store.delete_archived(deletion));
                    true
                })
        });
    }
}
