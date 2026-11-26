use super::*;

impl SessionsSidebar {
    /// Store changes and local disclosures invalidate the model. Scroll and
    /// navigation-animation frames only clone the shared snapshot; ListState
    /// measures the visible rows, including project captions of different height.
    /// With the `window` the list is drawn in, rows that move glide there
    /// from where they were.
    pub(super) fn compact_model(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> Rc<CompactListModel> {
        // The list is drawn again as this client's minute turns; each row
        // tells its time by its own host's clock.
        let now = now_secs(cx);
        let host_nows: Vec<(HostKey, u64)> = self
            .hosts
            .iter()
            .map(|host| (host.key.clone(), host.store.read(cx).host_now_secs(cx)))
            .collect();
        let host_now = move |host: &HostKey| {
            host_nows
                .iter()
                .find(|(key, _)| key == host)
                .map(|(_, now)| *now)
        };
        let locale = rust_i18n::locale();
        if self.compact_model_dirty
            || self
                .compact_model
                .as_ref()
                .is_none_or(|model| model.locale.as_str() != &*locale)
        {
            let swiped_off = self.swiped_off(cx);
            let lists = self.thread_lists(cx);
            let layout = DevicePreferences::sidebar_layout(cx);
            let kept = |host: &HostKey, meta: &SessionMeta| {
                !swiped_off
                    .iter()
                    .any(|thread| thread.host == *host && thread.session_id == meta.id)
            };
            // Each host's threads and groups, less those swiped off.
            let hosts: Vec<(HostKey, Vec<SessionMeta>, Vec<ProjectGroup>)> = lists
                .iter()
                .map(|lists| {
                    let mut sessions = (*lists.flat).clone();
                    sessions.retain(|meta| kept(&lists.host, meta));
                    let mut groups = (*lists.groups).clone();
                    for group in &mut groups {
                        group.sessions.retain(|meta| kept(&lists.host, meta));
                    }
                    (lists.host.clone(), sessions, groups)
                })
                .collect();
            let collapsed_projects: HashSet<ProjectKey> = hosts
                .iter()
                .filter_map(|(host, _, groups)| {
                    let store = self.store_of(host)?;
                    let store = store.read(cx);
                    Some(
                        groups
                            .iter()
                            .filter(|group| store.is_project_collapsed(&group.project.id))
                            .map(|group| ProjectKey {
                                host: host.clone(),
                                project_id: group.project.id.clone(),
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .flatten()
                .collect();
            let group_count: usize = hosts.iter().map(|(_, _, groups)| groups.len()).sum();
            let sectioned = self.sectioned();
            let host_row = |host: &HostKey, collapsed| {
                CompactListRow::Host(CompactHostRow {
                    host: host.clone(),
                    row_id: format!("compact-host-{}", host.name()).into(),
                    collapsed,
                })
            };

            let mut rows = Vec::new();
            let mut folds: Vec<f32> = Vec::new();
            let thread_rows = |visible: Vec<Listed>, recent: bool| {
                let mut rows = Vec::new();
                let last = visible.len().saturating_sub(1);
                for (index, (host, meta)) in visible.into_iter().enumerate() {
                    let (Some(store), Some(of)) = (
                        self.store_of(host),
                        lists.iter().find(|lists| lists.host == *host),
                    ) else {
                        continue;
                    };
                    let listed = self.listed((host, meta));
                    let state = self.thread_row_state(
                        store.read(cx),
                        &listed,
                        &of.flags,
                        format!("compact-thread-{}", listed.name),
                    );
                    let project_name = recent
                        .then(|| {
                            hosts.iter().find(|(other, _, _)| other == host).and_then(
                                |(_, _, groups)| {
                                    groups.iter().find(|group| {
                                        meta.project_id.as_deref()
                                            == Some(group.project.id.as_str())
                                    })
                                },
                            )
                        })
                        .flatten()
                        .map(|group| {
                            SharedString::from(crate::project_icon::label(&group.project))
                        });
                    let timestamp = if recent {
                        shown_timestamp(
                            meta,
                            of.flags
                                .get(&meta.id)
                                .map(|flags| flags.standing)
                                .unwrap_or_default(),
                        )
                    } else {
                        project_view_timestamp(meta)
                    };
                    let host_name = (recent && sectioned).then(|| store.read(cx).host_title());
                    rows.push(CompactListRow::Thread(Rc::new(CompactThreadRow {
                        row_id: format!("compact-thread-row-{}", listed.name).into(),
                        timestamp,
                        relative_time: humanize_ago(
                            store.read(cx).host_now_secs(cx).saturating_sub(timestamp),
                        )
                        .into(),
                        working: of.flags.get(&meta.id).is_some_and(|flags| flags.working),
                        state,
                        meta: meta.clone(),
                        project_name,
                        host_name,
                        separator: index != last,
                    })));
                }
                rows
            };
            if layout == SidebarLayout::Flat {
                let key = "recent";
                let shelf = Shelf::Recent;
                rows.extend(
                    lists
                        .iter()
                        .filter(|lists| lists.down)
                        .map(|lists| host_row(&lists.host, None)),
                );
                let ThreadSections {
                    pinned,
                    active,
                    settled,
                } = merged(
                    hosts
                        .iter()
                        .zip(lists.iter())
                        .map(|((host, sessions, _), lists)| {
                            tagged_sections(host, project_threads(sessions, None, &lists.hidden))
                        })
                        .collect(),
                    &lists,
                );
                let pinned_and_active_empty = pinned.is_empty() && active.is_empty();
                // A phone has no hover to reveal section labels, so they stay.
                if !pinned.is_empty() {
                    rows.push(CompactListRow::Caption {
                        key: format!("{key}-pinned"),
                        label: crate::tr!("sidebar.pinned_caption").into_owned().into(),
                    });
                    rows.extend(thread_rows(pinned, true));
                    if !active.is_empty() {
                        rows.push(CompactListRow::Caption {
                            key: format!("{key}-active"),
                            label: crate::tr!("sidebar.active_caption").into_owned().into(),
                        });
                    }
                }
                rows.extend(thread_rows(active, true));
                if pinned_and_active_empty && !settled.is_empty() {
                    rows.push(CompactListRow::Empty {
                        key: format!("{key}-empty"),
                    });
                }
                if !settled.is_empty() {
                    rows.push(CompactListRow::Settled {
                        key: key.into(),
                        shelf: shelf.clone(),
                        count: settled.len(),
                    });
                    let count = settled.len();
                    let mut visible = settled;
                    self.limit_settled_rows(&shelf, &mut visible);
                    let hidden = count - visible.len();
                    let more = self.settled_more_count(&shelf, hidden);
                    let (shelf_folds, more_factor) =
                        self.shelf_factors(&shelf, &visible, more.is_some());
                    folds.resize(rows.len(), 1.);
                    rows.extend(thread_rows(visible, true));
                    folds.extend(shelf_folds);
                    if let Some(count) = more {
                        rows.push(CompactListRow::More {
                            key: format!("{key}-more"),
                            shelf,
                            count,
                        });
                        folds.push(more_factor);
                    }
                }
            } else {
                for (host, _, groups) in &hosts {
                    let host_collapsed = DevicePreferences::host_collapsed(host, cx);
                    let host_key = fold_key(Fold::Host, host.name());
                    if sectioned {
                        folds.resize(rows.len(), 1.);
                        rows.push(host_row(host, Some(host_collapsed)));
                        folds.push(1.);
                        if !self.fold_shown(&host_key, !host_collapsed) {
                            continue;
                        }
                    }
                    let host_start = rows.len();
                    for group in groups {
                        let project = ProjectKey {
                            host: host.clone(),
                            project_id: group.project.id.clone(),
                        };
                        let name = self.name(host, &group.project.id).into_owned();
                        let key = name.as_str();
                        let count = group.sessions.len();
                        let has_header = sectioned || group_count > 1 || group.project.quick_chat;
                        let collapsed = has_header && collapsed_projects.contains(&project);
                        let start = rows.len();
                        let project_key = fold_key(Fold::Project, key);
                        if self.fold_shown(&project_key, !collapsed) {
                            folds.resize(rows.len(), 1.);
                            let (active, settled) = project_group_threads(&group.sessions);
                            rows.extend(thread_rows(tagged(host, active).collect(), false));
                            if !settled.is_empty() {
                                let count = settled.len();
                                let shelf = Shelf::Project(project.clone());
                                rows.push(CompactListRow::Settled {
                                    key: key.into(),
                                    shelf: shelf.clone(),
                                    count,
                                });
                                let mut settled: Vec<Listed> = tagged(host, settled).collect();
                                self.limit_settled_rows(&shelf, &mut settled);
                                let hidden = count - settled.len();
                                let more = self.settled_more_count(&shelf, hidden);
                                let (shelf_folds, more_factor) =
                                    self.shelf_factors(&shelf, &settled, more.is_some());
                                folds.resize(rows.len(), 1.);
                                rows.extend(thread_rows(settled, false));
                                folds.extend(shelf_folds);
                                if let Some(count) = more {
                                    rows.push(CompactListRow::More {
                                        key: format!("{key}-more"),
                                        shelf,
                                        count,
                                    });
                                    folds.push(more_factor);
                                }
                            }
                            folds.resize(rows.len(), 1.);
                            let progress = self.fold_progress(&project_key, !collapsed);
                            let count = rows.len() - start;
                            for (index, factor) in folds[start..].iter_mut().enumerate() {
                                *factor = nested_fold(fold_factor(progress, index, count), *factor);
                            }
                        }
                        if has_header {
                            folds.resize(start.max(folds.len()), 1.);
                            folds.insert(start, 1.);
                            rows.insert(
                                start,
                                CompactListRow::Project(CompactProjectRow {
                                    row_id: format!("compact-group-{name}").into(),
                                    id: name.clone(),
                                    project,
                                    name: crate::project_icon::group_label(&group.project).into(),
                                    label: if group.project.quick_chat {
                                        crate::tr!("sidebar.quick_chats").into_owned()
                                    } else {
                                        crate::tr!(
                                            "sidebar.project",
                                            name = group.project.name.clone()
                                        )
                                        .into_owned()
                                    }
                                    .into(),
                                    count: count.to_string().into(),
                                    collapsed,
                                }),
                            );
                        }
                    }
                    if sectioned {
                        folds.resize(rows.len(), 1.);
                        let progress = self.fold_progress(&host_key, !host_collapsed);
                        let count = rows.len() - host_start;
                        for (index, factor) in folds[host_start..].iter_mut().enumerate() {
                            *factor = nested_fold(fold_factor(progress, index, count), *factor);
                        }
                    }
                }
            }
            rows.push(CompactListRow::BottomInset);
            folds.resize(rows.len(), 1.);
            let all: Vec<(u64, f32)> = rows
                .iter()
                .zip(&folds)
                .map(|(row, factor)| (presence::row_key(row.key()), *factor))
                .collect();
            let mut kept = folds.iter().map(|factor| *factor >= 0.);
            rows.retain(|_| kept.next().unwrap_or(true));
            folds.retain(|factor| *factor >= 0.);
            let drawn = self.compact_model.clone().unwrap_or_default();
            self.compact_presence.update(
                &all,
                self.folding,
                &self.compact_list_state,
                |index| {
                    let flight = self.row_swipes.flight_of(drawn.rows.get(index), cx);
                    (drawn.clone(), index, flight)
                },
                cx,
            );
            self.reconcile_row_swipes(&rows, cx);
            let previous = |index: usize| {
                self.compact_model
                    .as_ref()
                    .and_then(|model| model.rows.get(index))
                    .map(CompactListRow::key)
            };
            if let Some(window) = window
                && !rows
                    .iter()
                    .map(CompactListRow::key)
                    .eq((0..self.compact_list_state.item_count()).filter_map(previous))
            {
                let keys: Vec<&str> = rows.iter().map(CompactListRow::key).collect();
                self.compact_flip
                    .replace(&self.compact_list_state, previous, &keys, window);
            }
            replace_list_rows(
                &self.compact_list_state,
                |index| {
                    self.compact_model
                        .as_ref()
                        .and_then(|model| model.rows.get(index))
                        // The loading/empty model contains only padding. Anchoring to
                        // it would open the first Index snapshot at the list's bottom.
                        .filter(|row| !matches!(row, CompactListRow::BottomInset))
                        .map(CompactListRow::key)
                },
                rows.iter().map(CompactListRow::key),
            );
            if let Some(id) = self.reveal_pinned.clone() {
                let pinned = rows
                    .iter()
                    .enumerate()
                    .find_map(|(index, row)| match row {
                        CompactListRow::Thread(row) if row.state.thread == id => Some((
                            index,
                            tcode_core::thread_sort::thread_section(&row.meta)
                                == ThreadSection::Pinned,
                        )),
                        _ => None,
                    })
                    .map(|(index, pinned)| {
                        // The PINNED caption above the row comes into view with it.
                        let caption =
                            index > 0 && matches!(rows[index - 1], CompactListRow::Caption { .. });
                        (index - usize::from(caption), pinned)
                    });
                self.reveal_pinned_row(
                    &self.compact_list_state.clone(),
                    &self.compact_scroll.clone(),
                    pinned,
                    cx,
                );
            }
            self.compact_model = Some(Rc::new(CompactListModel {
                rows,
                folds,
                has_rows: group_count > 0 || sectioned,
                locale: locale.to_string(),
                minute: now / 60,
            }));
            self.compact_model_dirty = false;
        }
        let model = self
            .compact_model
            .as_mut()
            .expect("compact model initialized");
        if model.minute != now / 60 {
            let model = Rc::make_mut(model);
            model.minute = now / 60;
            for row in &mut model.rows {
                if let CompactListRow::Thread(row) = row {
                    let row = Rc::make_mut(row);
                    if let Some(now) = host_now(&row.state.thread.host) {
                        row.relative_time = humanize_ago(now.saturating_sub(row.timestamp)).into();
                    }
                }
            }
        }
        model.clone()
    }

    /// Compact thread list with shared layout preference. Navigation replaces
    /// persistent row selection; desktop-only controls stay in the desktop list.
    /// The threads waiting for a connection, each folding in as it is
    /// asked for and away once the host has it.
    pub(super) fn pending_thread_rows(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut pending: Vec<(SharedString, (ThreadKey, String))> = Vec::new();
        for host in &self.hosts {
            let store = host.store.read(cx);
            let cached = store.sidebar_sessions();
            let loading = store.threads_loading();
            pending.extend(
                store
                    .pending_sessions()
                    .into_iter()
                    .filter(|(id, _)| loading || !cached.iter().any(|meta| &meta.id == id))
                    .map(|(id, preview)| {
                        let name = SharedString::from(self.name(&host.key, &id).into_owned());
                        let thread = ThreadKey {
                            host: host.key.clone(),
                            session_id: id,
                        };
                        (name, (thread, preview))
                    }),
            );
        }
        crate::motion::keyed(
            "pending-thread-rows",
            pending,
            ROWS,
            window,
            cx,
            |name, (thread, preview), _, cx| {
                let thread = thread.clone();
                crate::material::list_row(
                    SharedString::from(format!("pending-thread-{name}")),
                    format!("{}, {preview}", crate::tr!("chat.waiting_connection")).into(),
                    cx,
                )
                .debug_selector(|| "pending-thread-row".into())
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(cx.theme().tokens.spacing.xs)
                        .child(div().text_body(true, cx).child(crate::widgets::fade_text(
                            crate::tr!("chat.waiting_connection"),
                        )))
                        .child(
                            div()
                                .text_secondary(true, cx)
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(preview.clone())),
                        ),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(store) = this.store_of(&thread.host) {
                        store.update(cx, |store, cx| {
                            store.show_thread(thread.session_id.clone(), cx);
                        });
                    }
                }))
                .into_any_element()
            },
        )
        .render(v_flex().w_full())
    }

    pub(super) fn render_compact_row(
        &mut self,
        row: &CompactListRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match row {
            CompactListRow::Settled { shelf, count, .. } => {
                self.render_settled_header(shelf, *count, None, window, cx)
            }
            CompactListRow::Caption { label, .. } => {
                crate::material::list_caption(label.clone(), true, cx).into_any_element()
            }
            CompactListRow::More { shelf, count, .. } => {
                self.render_settled_more(shelf, *count, cx)
            }
            CompactListRow::Empty { .. } => self.render_active_empty(cx),
            CompactListRow::Host(row) => self.render_compact_host_heading(row, window, cx),
            CompactListRow::Project(row) => self
                .render_compact_group_header(row, window, cx)
                .into_any_element(),
            CompactListRow::Thread(row) => {
                let content = self.compact_thread_content(row, window, cx);
                self.swipeable_row(row, content, window, cx)
            }
            CompactListRow::BottomInset => div().h(cx.theme().tokens.spacing.xl).into_any_element(),
        }
    }

    pub(super) fn compact_thread_content(
        &mut self,
        row: &CompactThreadRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        v_flex()
            .w_full()
            .child(self.render_compact_thread(row, window, cx))
            .when(row.separator, |list| {
                list.child(crate::material::row_hairline(px(row::PHONE_ROW_INSET), cx))
            })
            .into_any_element()
    }

    pub(super) fn render_compact(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let full_scope = self.full_scope(cx);
        self.tidy_row_swipes(cx);
        let model = self.compact_model(Some(window), cx);
        let layout = DevicePreferences::sidebar_layout(cx);
        let body = self.loading_body(
            move |this, window, cx| {
                // The empty page fades in where the last project's list was.
                let empty = crate::motion::shown(
                    "threads-empty",
                    (!model.has_rows).then_some(()),
                    SHOW,
                    window,
                    cx,
                );
                if let Some(empty) = empty.filter(|_| !model.has_rows) {
                    div()
                        .id("threads-empty")
                        .opacity(empty.progress())
                        .debug_selector(|| "threads-empty".into())
                        .size_full()
                        .child(crate::material::empty_state(
                            Icon::new(IconName::Folder),
                            if full_scope {
                                crate::tr!("mobile.projects_empty")
                            } else {
                                crate::tr!("member.empty_title")
                            },
                            if full_scope {
                                crate::tr!("mobile.projects_help")
                            } else {
                                crate::tr!("member.empty_description")
                            },
                            cx,
                        ))
                        .into_any_element()
                } else {
                    let reveal = std::mem::take(&mut this.compact_reveal_active)
                        .then(|| {
                            let active = this.current_row(cx);
                            model.rows.iter().position(|row| {
                                matches!(row, CompactListRow::Thread(row) if Some(&row.state.thread) == active.as_ref())
                            })
                        })
                        .flatten();
                    let reveal_model = model.clone();
                    this.compact_scroll.step(&this.compact_list_state, window, cx);
                    this.compact_presence
                        .settle(&this.compact_list_state, window, cx);
                    div()
                        .id("compact-thread-list")
                        .debug_selector(|| "compact-thread-list".into())
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .when_some(reveal, |list, index| {
                            list.child(
                                gpui::canvas(
                                    cx.processor(
                                        move |this, bounds: gpui::Bounds<gpui::Pixels>, window, cx| {
                                            // Child autoscroll retries prepaint without rolling back
                                            // accessibility nodes in gpui-fast 3755a55. Measure the
                                            // lead using this frame's bounds before the list prepaints.
                                            let mut remaining = bounds.size.height / 3.;
                                            let mut first = index;
                                            while first > 0 && remaining > px(0.) {
                                                first -= 1;
                                                remaining -= this
                                                    .render_compact_row(
                                                        &reveal_model.rows[first],
                                                        window,
                                                        cx,
                                                    )
                                                    .layout_as_root(
                                                        gpui::size(
                                                            gpui::AvailableSpace::Definite(
                                                                bounds.size.width,
                                                            ),
                                                            gpui::AvailableSpace::MinContent,
                                                        ),
                                                        window,
                                                        cx,
                                                    )
                                                    .height;
                                            }
                                            this.compact_list_state.scroll_to(gpui::ListOffset {
                                                item_ix: first,
                                                offset_in_item: (-remaining).max(px(0.)),
                                            });
                                        },
                                    ),
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .size_full(),
                            )
                        })
                        .child(crate::scroll::page_viewport(
                            "compact-thread-bounce",
                            &this.compact_list_state,
                            list(
                                this.compact_list_state.clone(),
                                cx.processor(move |this, index: usize, window, cx| {
                                    let factor = model.folds.get(index).copied().unwrap_or(1.);
                                    let name = model.rows[index].key();
                                    let folded = this.fold_row(SharedString::from(format!("compact-fold-{name}")), factor, window, cx, |this, window, cx| {
                                        this.render_compact_row(&model.rows[index], window, cx)
                                    });
                                    let key = presence::row_key(name);
                                    // The row's content glides to its new place; what folds away against it stays with its layout.
                                    let folded = this.compact_flip.row(
                                        index,
                                        SharedString::from(format!("compact-{name}")),
                                        folded,
                                    );
                                    this.compact_presence.around(key, cx).draw(
                                        "compact-row",
                                        key,
                                        Glide::Rest,
                                        folded,
                                        |(model, index, flight), window| {
                                            this.render_left_compact_row(&model.rows[*index], *flight, window, cx)
                                        },
                                        window,
                                    )
                                }),
                            )
                            .size_full(),
                        ))
                        .when(!window.is_inspector_picking(cx), |list| {
                            list.child(thread_list_scrollbar(
                                "compact-thread-scrollbar",
                                &this.compact_list_state,
                                COMPACT_THREAD_ROW_HEIGHT,
                            ))
                        })
                        .into_any_element()
                }
            },
            window,
            cx,
        );

        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_link_pull_request))
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_regenerate_title))
            .on_action(cx.listener(Self::on_stop_title))
            .on_action(cx.listener(Self::on_fork))
            .on_action(cx.listener(Self::on_merge_worktree))
            .on_action(cx.listener(Self::on_move_to_project))
            .on_action(cx.listener(Self::on_mark_unread))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_copy_id))
            .on_action(cx.listener(Self::on_export_jsonl))
            .on_action(cx.listener(Self::on_export_markdown))
            .on_action(cx.listener(Self::on_settle))
            .on_action(cx.listener(Self::on_auto_settle))
            .on_action(cx.listener(Self::on_follow_up))
            .on_action(cx.listener(Self::on_park))
            .on_action(cx.listener(Self::on_stop_watches))
            .on_action(cx.listener(Self::on_make_active))
            .on_action(cx.listener(Self::on_pin))
            .on_action(cx.listener(Self::on_unpin))
            .on_action(cx.listener(Self::on_move))
            .on_action(cx.listener(Self::on_arrange))
            .on_action(cx.listener(Self::on_archive))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_add_project))
            .on_action(cx.listener(Self::on_sort_projects))
            .on_action(cx.listener(Self::on_manage_hosts))
            .on_action(cx.listener(Self::on_host_settings))
            .on_action(cx.listener(Self::on_leave_host))
            .child(self.render_compact_search(cx))
            .child(
                bar("compact-layout-bar", Bar::Section, true, cx)
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_secondary(true, cx)
                            .child(crate::widgets::fade_text(match layout {
                                SidebarLayout::Flat => crate::tr!("sidebar.recent"),
                                SidebarLayout::Grouped => crate::tr!("sidebar.by_project"),
                            })),
                    )
                    .child(
                        div()
                            .debug_selector(|| "compact-layout-toggle".into())
                            .child(self.render_layout_toggle(layout, cx)),
                    ),
            )
            .when(!self.threads_loading(cx), |list| {
                list.child(self.pending_thread_rows(window, cx))
            })
            .child(body)
            .into_any_element()
    }

    pub(super) fn render_compact_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let spacing = &cx.theme().tokens.spacing;
        let (top, bottom) = (spacing.sm, spacing.xs);
        div()
            .flex_none()
            .px(px(COMPACT_PAGE_INSET))
            .pt(top)
            .pb(bottom)
            .child(self.search_field(
                "compact-search",
                crate::tr!("mobile.search_threads").into(),
                true,
                cx,
            ))
    }

    /// A host's heading in the phone's list over several hosts: a section's
    /// caption, at body size since projects are captions under it.
    pub(super) fn render_compact_host_heading(
        &self,
        row: &CompactHostRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(parts) = self.host_heading_parts(&row.host, true, window, cx) else {
            return div().into_any_element();
        };
        let id = row.host.name().to_owned();
        let toggle = row.host.clone();
        let spacing = &cx.theme().tokens.spacing;
        let heading = crate::material::accessible_clickable(
            h_flex(),
            row.row_id.clone(),
            Role::Button,
            crate::tr!("sidebar.host", name = parts.title.clone()),
            cx,
        )
        .debug_selector({
            let id = id.clone();
            move || format!("compact-host-{id}")
        })
        .w_full()
        .items_center()
        .gap(spacing.sm)
        .text_body(true, cx)
        .font_semibold()
        .child(crate::material::status_dot(parts.dot))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(crate::widgets::fade_text(parts.title)),
        )
        .children(parts.mark)
        .children(parts.pair)
        .child(parts.menu)
        .when_some(row.collapsed, |heading, collapsed| {
            heading
                .aria_expanded(!collapsed)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_host(&toggle, cx)))
                .child(
                    collapse_chevron(
                        self.fold_progress(&fold_key(Fold::Host, &id), !collapsed),
                        cx,
                    )
                    .text_color(cx.theme().foreground),
                )
        });
        v_flex()
            .w_full()
            .px(px(COMPACT_PAGE_INSET))
            .pt(spacing.lg)
            .pb(spacing.xs)
            .child(heading)
            .children(parts.status)
            .into_any_element()
    }

    pub(super) fn render_compact_group_header(
        &self,
        row: &CompactProjectRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let project = row.project.clone();
        let collapsed = row.collapsed;
        let spacing = &cx.theme().tokens.spacing;
        crate::material::accessible_clickable(
            h_flex(),
            row.row_id.clone(),
            Role::Button,
            row.label.clone(),
            cx,
        )
        .aria_expanded(!collapsed)
        .debug_selector(|| "compact-group-header".into())
        // A project is a section of the thread list, so its header is a list
        // caption that also carries the collapse affordance; a clickable row
        // rather than a `Button`, which centres its content.
        .w_full()
        .px(px(COMPACT_PAGE_INSET))
        .pt(spacing.lg)
        .pb(spacing.xs)
        .items_center()
        .gap(spacing.sm)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_project(&project, cx);
        }))
        .text_caption(cx)
        .when_some(
            self.store_of(&row.project.host).and_then(|store| {
                let store = store.read(cx);
                let project = store.project(&row.project.project_id)?;
                Some(crate::project_icon::artwork(
                    store.image_host(),
                    project,
                    14.,
                ))
            }),
            |el, artwork| el.child(artwork),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .debug_selector({
                    let name = row.name.clone();
                    move || format!("compact-group-{name}")
                })
                .overflow_hidden()
                .child(crate::widgets::fade_text(row.name.clone())),
        )
        .children(self.shared_badge(&row.project, window, cx))
        .child(div().flex_none().child(row.count.clone()))
        .child(
            collapse_chevron(
                self.fold_progress(&fold_key(Fold::Project, &row.id), !collapsed),
                cx,
            )
            .text_color(cx.theme().foreground),
        )
    }

    /// One thread row. Long press opens the shared thread context menu.
    pub(super) fn render_compact_thread(
        &self,
        cached: &CompactThreadRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let meta = &cached.meta;
        // A rename does not rebuild the list's model: the row reads it live.
        let state = &ThreadRowState {
            renaming: self
                .renaming
                .as_ref()
                .filter(|rename| rename.thread == cached.state.thread)
                .map(|rename| rename.input.clone()),
            ..cached.state.clone()
        };
        let Some(store) = self.store_of(&state.thread.host) else {
            return div().into_any_element();
        };
        let working = cached.working;
        let name = state.name.clone();
        let click = state.thread.clone();
        let shows = row::RowShows::of(
            store.read(cx),
            meta,
            state,
            working,
            cached.project_name.clone(),
            cached.host_name.clone(),
        );
        let fill = transition(
            (cached.row_id.clone(), "fill"),
            row::row_fill(RowLight::None, shows.pinned, cx),
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        // The row is the touch target, so it speaks for its badge too.
        let label = crate::i18n::join_list(
            std::iter::once(crate::tr!("sidebar.thread", title = meta.title.clone()).into_owned())
                .chain(shows.details())
                .chain(crate::pull_requests::badge_label(&meta.pull_requests, cx)),
        );
        let time = div()
            .flex_none()
            .whitespace_nowrap()
            .child(crate::widgets::fade_text(cached.relative_time.clone()))
            .into_any_element();
        let lines = self.row_lines(&store, meta, state, &shows, time, true, window, cx);
        let row = crate::material::list_row(cached.row_id.clone(), label.into(), cx)
            .debug_selector({
                let name = name.clone();
                move || format!("compact-row-{name}")
            })
            .h(px(row::PHONE_ROW_HEIGHT))
            .px(px(row::PHONE_ROW_INSET))
            .py_0()
            .bg(fill)
            .on_click(cx.listener(move |this, _, _, cx| {
                if this
                    .renaming
                    .as_ref()
                    .is_some_and(|rename| rename.thread == click)
                {
                    return;
                }
                let Some(store) = this.store_of(&click.host) else {
                    return;
                };
                let id = &click.session_id;
                if store.read(cx).is_thread_open(id) && store.read(cx).session_loading_for(id) {
                    return;
                }
                store.update(cx, |store, cx| {
                    store.show_thread(click.session_id.clone(), cx);
                });
            }))
            .child(lines.flex_1());
        self.thread_context_menu(row, meta, state, working, true, cx)
    }
}
