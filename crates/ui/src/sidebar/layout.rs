use super::*;

impl SessionsSidebar {
    /// The Grouped layout's header and list.
    pub(super) fn render_grouped_layout(
        &mut self,
        lead: &Entity<WorkspaceStore>,
        lists: ThreadLists,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (gpui::AnyElement, gpui::AnyElement) {
        let mut rows = self.grouped_rows_folding(&lists, cx);
        if lists.iter().all(|lists| lists.groups.is_empty()) {
            let hint = if self.full_scope(cx) {
                crate::tr!("sidebar.empty")
            } else {
                crate::tr!("member.empty_description")
            };
            rows.push((GroupedListRow::Hint(hint), 1.));
        }
        let all: Vec<(u64, f32)> = rows
            .iter()
            .map(|(row, factor)| (presence::row_key(row.key(&lists)), *factor))
            .collect();
        rows.retain(|(_, factor)| *factor >= 0.);
        let keys = rows.iter().map(|(row, _)| row.key(&lists));
        let drawn = self.grouped_drawn.clone();
        self.grouped_presence.update(
            &all,
            self.folding,
            &self.grouped_list_state,
            |index| (drawn.clone(), index),
            cx,
        );
        if !keys.clone().eq(self.grouped_row_keys.iter().map(drawn_key)) {
            replace_gliding_rows(
                &self.grouped_list_state,
                &self.grouped_flip,
                |index| self.grouped_row_keys.get(index).map(drawn_key),
                keys.clone(),
                window,
            );
            self.grouped_row_keys = keys.map(|(kind, id)| (kind, id.into_owned())).collect();
        }
        self.grouped_presence
            .settle(&self.grouped_list_state, window, cx);
        let (rows, folds) = rows.into_iter().unzip();
        let frame = Rc::new(GroupedFrame { rows, folds, lists });
        self.grouped_drawn = frame.clone();
        let flip = self.grouped_flip.clone();
        let thread_list = list(
            self.grouped_list_state.clone(),
            cx.processor(move |this, index: usize, window, cx| {
                let Some(row) = frame.rows.get(index) else {
                    return div().into_any_element();
                };
                let factor = frame.folds.get(index).copied().unwrap_or(1.);
                let name = row.key(&frame.lists);
                let fold_id = SharedString::from(format!("grouped-fold-{}-{}", name.0, name.1));
                let folded = this.fold_row(fold_id, factor, window, cx, |this, window, cx| {
                    this.render_grouped_row(index, row, &frame.lists, window, cx)
                });
                let key = presence::row_key(&name);
                // The row's content glides to its new place; what folds away against it stays with its layout.
                let folded = flip.row(
                    index,
                    SharedString::from(format!("grouped-{}-{}", name.0, name.1)),
                    folded,
                );
                this.grouped_presence.around(key, cx).draw(
                    "grouped-row",
                    key,
                    Glide::Rest,
                    folded,
                    |(frame, index): &(Rc<GroupedFrame>, usize), window| {
                        let Some(row) = frame.rows.get(*index) else {
                            return div().into_any_element();
                        };
                        this.render_grouped_row(*index, row, &frame.lists, window, cx)
                    },
                    window,
                )
            }),
        )
        .flex_1()
        .min_h_0()
        .pb_2();
        let thread_list = v_flex()
            .id("grouped-thread-list")
            .flex_1()
            .min_h_0()
            .relative()
            .child(crate::scroll::page_viewport(
                "grouped-thread-bounce",
                &self.grouped_list_state,
                thread_list,
            ))
            .when(!window.is_inspector_picking(cx), |list| {
                list.child(thread_list_scrollbar(
                    "grouped-thread-scrollbar",
                    &self.grouped_list_state,
                    crate::material::selectable_row_height(false),
                ))
            })
            .into_any_element();
        (
            self.render_projects_header(lead, cx).into_any_element(),
            thread_list,
        )
    }

    /// The Flat layout's header and list.
    pub(super) fn render_flat_layout(
        &mut self,
        lists: ThreadLists,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (gpui::AnyElement, gpui::AnyElement) {
        let scope = self.project_filter.clone();
        let ThreadRows {
            pinned,
            active,
            settled: settled_visible,
            settled_count,
            settled_hidden_count,
        } = self.thread_rows(&lists, scope.as_ref(), &Shelf::Recent);
        let mut visible: Vec<FlatListRow> = lists
            .iter()
            .filter(|lists| lists.down)
            .map(|lists| FlatListRow::Host(lists.host.clone()))
            .collect();
        for (section, rows) in [
            (ThreadSection::Pinned, &pinned),
            (ThreadSection::Active, &active),
        ] {
            visible.push(FlatListRow::Boundary(section, rows.is_empty()));
            for row in rows.iter() {
                visible.push(FlatListRow::Thread(Box::new(self.listed(*row))));
            }
        }
        let more = self.settled_more_count(&Shelf::Recent, settled_hidden_count);
        let (shelf, more_factor) =
            self.shelf_factors(&Shelf::Recent, &settled_visible, more.is_some());
        let mut folds = vec![1.; visible.len()];
        if pinned.is_empty() && active.is_empty() && settled_count == 0 {
            // An active project filter, or spaces hiding every project with
            // threads, can empty the list while threads exist; those states
            // get their own hint, not the no-projects one.
            let no_threads = lists.iter().all(|lists| lists.flat.is_empty());
            let hint = if !self.full_scope(cx) && no_threads {
                if self
                    .hosts
                    .iter()
                    .all(|host| host.store.read(cx).projects().is_empty())
                {
                    crate::tr!("member.empty_description")
                } else {
                    crate::tr!("member.threads_empty")
                }
            } else if no_threads {
                crate::tr!("sidebar.empty")
            } else if scope.is_none() {
                crate::tr!("sidebar.shared_threads_hidden")
            } else {
                crate::tr!("sidebar.filter_empty")
            };
            visible.push(FlatListRow::Hint(hint));
            folds.push(1.);
        }
        if settled_count > 0 {
            if pinned.is_empty() && active.is_empty() {
                visible.push(FlatListRow::Empty);
            }
            visible.push(FlatListRow::Settled);
            folds.resize(visible.len(), 1.);
            for (row, factor) in settled_visible.into_iter().zip(shelf) {
                visible.push(FlatListRow::Thread(Box::new(self.listed(row))));
                folds.push(factor);
            }
        }
        if let Some(count) = more {
            visible.push(FlatListRow::More(count));
            folds.push(more_factor);
        }
        let all: Vec<(u64, f32)> = visible
            .iter()
            .zip(&folds)
            .map(|(row, factor)| (presence::row_key(row.key()), *factor))
            .collect();
        let mut kept = folds.iter().map(|factor| *factor >= 0.);
        visible.retain(|_| kept.next().unwrap_or(true));
        folds.retain(|factor| *factor >= 0.);
        let keys = visible.iter().map(FlatListRow::key);
        let drawn = self.flat_drawn.clone();
        self.flat_presence.update(
            &all,
            self.folding,
            &self.flat_list_state,
            |index| (drawn.clone(), index),
            cx,
        );
        if !keys.clone().eq(self
            .flat_row_keys
            .iter()
            .map(|(kind, id)| (*kind, id.as_str())))
        {
            replace_gliding_rows(
                &self.flat_list_state,
                &self.flat_flip,
                |index| {
                    self.flat_row_keys
                        .get(index)
                        .map(|(kind, id)| (*kind, id.as_str()))
                },
                keys.clone(),
                window,
            );
            self.flat_row_keys = keys.map(|(kind, id)| (kind, id.to_owned())).collect();
        }
        self.flat_presence.settle(&self.flat_list_state, window, cx);
        let pinned_row = |thread: &ThreadKey| {
            let index = visible
                .iter()
                .position(|row| matches!(row, FlatListRow::Thread(row) if row.key() == *thread))?;
            Some((
                index,
                pinned
                    .iter()
                    .any(|(host, meta)| **host == thread.host && meta.id == thread.session_id),
            ))
        };
        if let Some(id) = self.reveal_pinned.clone() {
            self.reveal_pinned_row(
                &self.flat_list_state.clone(),
                &self.flat_scroll.clone(),
                pinned_row(&id),
                cx,
            );
        }
        let project_names = lists
            .iter()
            .flat_map(|lists| {
                lists.groups.iter().map(|group| {
                    let project = ProjectKey {
                        host: lists.host.clone(),
                        project_id: group.project.id.clone(),
                    };
                    (project, crate::project_icon::label(&group.project))
                })
            })
            .collect::<HashMap<_, _>>();
        let frame = Rc::new(FlatFrame {
            rows: visible,
            folds,
            scope,
            settled_count,
            project_names,
            lists,
        });
        self.flat_drawn = frame.clone();
        let flip = self.flat_flip.clone();
        self.flat_scroll.step(&self.flat_list_state, window, cx);
        let thread_list = list(
            self.flat_list_state.clone(),
            cx.processor(move |this, index: usize, window, cx| {
                let factor = frame.folds.get(index).copied().unwrap_or(1.);
                let name = frame.rows.get(index).map_or(("", ""), FlatListRow::key);
                let fold_id = SharedString::from(format!("flat-fold-{}-{}", name.0, name.1));
                let folded = this.fold_row(fold_id, factor, window, cx, |this, window, cx| {
                    this.render_flat_row(&frame, index, window, cx)
                });
                let key = presence::row_key(name);
                // The row's content glides to its new place; what folds away against it stays with its layout.
                let folded = flip.row(
                    index,
                    SharedString::from(format!("flat-{}-{}", name.0, name.1)),
                    folded,
                );
                this.flat_presence.around(key, cx).draw(
                    "flat-row",
                    key,
                    Glide::Rest,
                    folded,
                    |(frame, index): &(Rc<FlatFrame>, usize), window| {
                        this.render_flat_row(frame, *index, window, cx)
                    },
                    window,
                )
            }),
        )
        .flex_1()
        .min_h_0()
        .into_any_element();
        (
            self.render_flat_header(window, cx).into_any_element(),
            v_flex()
                .id("flat-thread-list")
                .flex_1()
                .min_h_0()
                .relative()
                .on_drop(cx.listener(|this, _: &AnyDrag, window, cx| this.drop_thread(window, cx)))
                .on_drag_move(
                    cx.listener(|this, event: &gpui::DragMoveEvent<AnyDrag>, _, cx| {
                        this.drag_left_list(event, cx);
                        this.drag_auto_scroll(event, cx)
                    }),
                )
                .child(crate::scroll::page_viewport(
                    "flat-thread-bounce",
                    &self.flat_list_state,
                    thread_list,
                ))
                .when(!window.is_inspector_picking(cx), |list| {
                    list.child(thread_list_scrollbar(
                        "flat-thread-scrollbar",
                        &self.flat_list_state,
                        FLAT_ROW_HEIGHT,
                    ))
                })
                .into_any_element(),
        )
    }

    /// Row `index` of a frame of the Flat list.
    pub(super) fn render_flat_row(
        &mut self,
        frame: &FlatFrame,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(row) = frame.rows.get(index) else {
            return div().into_any_element();
        };
        let scope = frame.scope.clone();
        let listed = match row {
            FlatListRow::Host(host) => {
                return div()
                    .w_full()
                    .px_2()
                    .pb(px(2.))
                    .child(self.render_host_heading(host, None, window, cx))
                    .into_any_element();
            }
            FlatListRow::Thread(listed) => listed,
            FlatListRow::Boundary(section, empty) => {
                return div()
                    .w_full()
                    .px_2()
                    .child(self.render_drag_boundary(*section, *empty, scope, window, cx))
                    .into_any_element();
            }
            FlatListRow::Settled => {
                let header = self.render_settled_header(
                    &Shelf::Recent,
                    frame.settled_count,
                    scope.as_ref(),
                    window,
                    cx,
                );
                return self
                    .drop_zone(div().w_full().child(header), DropZone::Settled, scope, cx)
                    .into_any_element();
            }
            FlatListRow::More(count) => {
                return self.render_settled_more(&Shelf::Recent, *count, cx);
            }
            FlatListRow::Empty => return self.render_active_empty(cx),
            FlatListRow::Hint(hint) => {
                return div()
                    .w_full()
                    .px_2()
                    .pb_2()
                    .child(empty_row(hint.clone(), cx))
                    .into_any_element();
            }
        };
        // While carried, the row keeps its place, hidden under its ghost.
        let project_name = listed
            .meta
            .project_id
            .as_ref()
            .and_then(|project_id| {
                frame.project_names.get(&ProjectKey {
                    host: listed.host.clone(),
                    project_id: project_id.clone(),
                })
            })
            .cloned();
        let Some(of) = frame.lists.iter().find(|lists| lists.host == listed.host) else {
            return div().into_any_element();
        };
        let host_name = self
            .store_of(&listed.host)
            .filter(|_| self.sectioned())
            .map(|store| store.read(cx).host_title());
        let thread =
            self.render_flat_thread(listed, &of.flags, project_name, host_name, window, cx);
        let row = div()
            .w_full()
            .px_2()
            .pb(px(row::DESKTOP_ROW_GAP))
            .child(drag_ghost::slot(
                arrange::thread_slot(&listed.name),
                thread,
                window,
                cx,
            ));
        let zone = DropZone::Row {
            thread: listed.key(),
            section: tcode_core::thread_sort::thread_section(&listed.meta),
        };
        self.drop_zone(row, zone, scope, cx).into_any_element()
    }
}
