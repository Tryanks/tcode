use super::*;

impl SessionsSidebar {
    /// What the chat column shows now, drawn from the next frame.
    pub(crate) fn set_shown(this: &Entity<Self>, shown: ShownThreads, cx: &mut App) {
        if this.read(cx).shown != shown {
            this.update(cx, |sidebar, cx| {
                sidebar.shown = shown;
                cx.notify();
            });
        }
    }

    /// The thread whose row the pointer is over.
    pub(crate) fn hovered_thread(&self) -> Option<ThreadKey> {
        let row = self.hover.hovered()?;
        let thread = row
            .strip_prefix("flat-thread-")
            .or_else(|| row.strip_prefix("thread-"))?;
        self.thread_named(thread)
    }

    /// The map of the panes, on a row whose thread one shows: the column's
    /// panes in a tiny grid, each laid out as it lies, those showing this
    /// thread filled. It widens into the row as it comes, the title giving it
    /// room gradually, and narrows away as it goes.
    pub(super) fn pane_glyph(
        &self,
        state: &ThreadRowState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let row = &state.thread;
        let map = self.shown.cells(row).next().map(|_| self.shown.map.clone());
        let id = SharedString::from(format!("pane-glyph-{}", state.name));
        let shown = crate::motion::shown(id.clone(), map, SHOW, window, cx)?;
        let (filled, other) = {
            let theme = cx.theme();
            (
                theme.sidebar_foreground,
                theme.muted_foreground.opacity(0.35),
            )
        };
        let mut glyph = div()
            .relative()
            .flex_none()
            .w(px(GLYPH_WIDTH))
            .h(px(GLYPH_HEIGHT));
        for cell in shown.value.iter() {
            let fill = transition(
                (id.clone(), format!("cell-{}", cell.pane.as_u64())),
                if cell.row.as_ref() == Some(row) {
                    filled
                } else {
                    other
                },
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            );
            // Each edge on a half point, a half point in from the grid
            // line, so neighbours stay apart at any share of the column.
            let edge = |share: f32, length: f32| (share * length * 2.).round() / 2.;
            let rect = cell.rect;
            let left = edge(rect.origin.x, GLYPH_WIDTH) + 0.5;
            let right = edge(rect.origin.x + rect.size.width, GLYPH_WIDTH) - 0.5;
            let top = edge(rect.origin.y, GLYPH_HEIGHT) + 0.5;
            let bottom = edge(rect.origin.y + rect.size.height, GLYPH_HEIGHT) - 0.5;
            glyph = glyph.child(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px((right - left).max(0.)))
                    .h(px((bottom - top).max(0.)))
                    .rounded(px(1.))
                    .bg(fill),
            );
        }
        Some(shown.fold_across_gap(ROW_GAP, glyph))
    }

    /// The row of the current pane's thread.
    pub(super) fn current_row(&self, cx: &App) -> Option<ThreadKey> {
        let thread = crate::shell::current_thread(cx)?;
        let store = self.store_of(&thread.host)?;
        let session_id = store.read(cx).roster_row(&thread.session_id);
        Some(ThreadKey {
            host: thread.host,
            session_id,
        })
    }

    /// `base` carries the row's selectable surface, selected when `light` is
    /// [`RowLight::Current`]; a `pinned` row rests on a faint fill. Its
    /// accessible label says `details` after the thread's title.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn thread_clickable_row(
        &self,
        base: gpui::Div,
        meta: &SessionMeta,
        state: &ThreadRowState,
        light: RowLight,
        pinned: bool,
        details: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> crate::material::Clickable<gpui::Div> {
        let thread = state.thread.clone();
        let row_id = SharedString::from(format!("{}-row", state.row_key));
        // The selected fill glides from the row left to the row chosen.
        let fill = transition(
            (row_id.clone(), "selected-fill"),
            row::row_fill(light, pinned, cx),
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let places: Vec<_> = self
            .shown
            .cells(&state.thread)
            .map(MapCell::place)
            .collect();
        let head = if places.is_empty() {
            crate::tr!("sidebar.thread", title = meta.title.clone())
        } else {
            crate::tr!(
                "sidebar.thread_in_pane",
                title = meta.title.clone(),
                place = crate::i18n::join_list(places)
            )
        }
        .into_owned();
        let label = if details.is_empty() {
            head
        } else {
            crate::i18n::join_list(std::iter::once(head).chain(details))
        };
        let row =
            crate::material::accessible_clickable(base.bg(fill), row_id, Role::Button, label, cx)
                .text_body(false, cx)
                .aria_selected(light == RowLight::Current)
                .debug_selector({
                    let name = state.name.clone();
                    move || format!("sidebar-thread-{name}")
                })
                .cursor_pointer()
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if event.modifiers().alt && !this.compact(cx) {
                            window.dispatch_action(
                                Box::new(crate::panes::OpenInNewPane(thread.clone())),
                                cx,
                            );
                            return;
                        }
                        let Some(store) = this.store_of(&thread.host) else {
                            return;
                        };
                        this.compact_model_dirty = true;
                        store.update(cx, |store, cx| {
                            store.show_thread(thread.session_id.clone(), cx);
                        });
                        cx.notify();
                    }),
                );
        let row = self.track_hover(row, &state.row_key);
        let row = match self
            .dragged_thread(meta, state, cx)
            .filter(|_| !self.compact(cx))
        {
            Some(dragged) => Self::drag_source(row, dragged, cx),
            None => row,
        };
        row.when(state.waiting_for_approval, |row| {
            row.with_tooltip(|_, _| {
                Tooltip::new(crate::tr!("sidebar.waiting_approval_tooltip").into_owned())
            })
        })
        .when(
            state.waiting_for_input && !state.waiting_for_approval,
            |row| {
                row.with_tooltip(|_, _| {
                    Tooltip::new(crate::tr!("sidebar.waiting_input_tooltip").into_owned())
                })
            },
        )
    }

    pub(super) fn rename_input(
        &self,
        input: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        div()
            .flex_1()
            .min_w_0()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.cancel_rename(cx)))
            .child(Input::new(input))
    }

    pub(super) fn thread_title_or_input(
        &self,
        meta: &SessionMeta,
        state: &ThreadRowState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let title = if let Some(input) = &state.renaming {
            self.rename_input(input, cx).into_any_element()
        } else {
            sidebar_label(meta.title.clone())
                // As wide as the title, so the row knows what it cuts.
                .flex_auto()
                .text_body(false, cx)
                .text_color(if meta.is_settled() {
                    cx.theme().muted_foreground
                } else {
                    cx.theme().sidebar_foreground
                })
                .into_any_element()
        };
        h_flex()
            .flex_auto()
            .min_w_0()
            .gap(px(6.))
            .child(title)
            .when_some(
                Self::fading_spinner(
                    format!("title-spinner-{}", state.row_key),
                    state.title_generating,
                    window,
                    cx,
                ),
                |row, spinner| row.child(spinner),
            )
    }

    /// The spinner beside a title while it generates, faded in and out.
    pub(super) fn fading_spinner(
        id: String,
        spinning: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let shown = crate::motion::shown(id, spinning.then_some(()), SHOW, window, cx)?;
        Some(shown.fade(Spinner::new().color(cx.theme().muted_foreground)))
    }

    /// A pinned row's pin; on row hover it becomes the Unpin button.
    /// A thread on Follow-up: attention the user asked for, not activity.
    /// It widens into the row as it is set, and narrows away as it is
    /// cleared, so the title beside it gives way gradually.
    pub(super) fn follow_up_glyph(
        meta: &SessionMeta,
        name: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let shown = crate::motion::shown(
            SharedString::from(format!("follow-up-shown-{name}")),
            meta.follow_up.is_some().then_some(()),
            SHOW,
            window,
            cx,
        )?;
        Some(
            shown.fold_across_gap(
                ROW_GAP,
                div()
                    .id(SharedString::from(format!("follow-up-{name}")))
                    .flex_none()
                    .child(Icon::new(IconName::Bell).text_color(cx.theme().muted_foreground))
                    .with_tooltip(|_, _| Tooltip::new(crate::tr!("control.follow_up.marker"))),
            ),
        )
    }

    /// A thread's PR badge, widening in beside what precedes it (across
    /// `gap`) when a PR is linked and narrowing away when the last goes or
    /// the title is renamed over it; `quiet`, the number alone in its line's
    /// muted text.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn pr_badge(
        store: &Entity<WorkspaceStore>,
        meta: &SessionMeta,
        name: &str,
        show: bool,
        gap: gpui::Pixels,
        slot: &'static str,
        quiet: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let links = show
            .then(|| meta.pull_requests.clone())
            .filter(|links| crate::pull_requests::badge_label(links, cx).is_some());
        let shown = crate::motion::shown(
            SharedString::from(format!("{slot}-{name}")),
            links,
            SHOW,
            window,
            cx,
        )?;
        let badge =
            crate::pull_requests::sidebar_badge(&shown.value, &meta.id, store.clone(), quiet, cx)?;
        Some(shown.fold_across_gap(gap, badge))
    }

    pub(super) fn pin_glyph(
        &self,
        meta: &SessionMeta,
        state: &ThreadRowState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        if meta.pinned_at.is_none() || meta.is_settled() {
            return None;
        }
        let thread = state.thread.clone();
        let label = crate::tr!("sidebar.unpin_tooltip");
        let button_id =
            gpui::ElementId::from(SharedString::from(format!("unpin-thread-{}", state.name)));
        let (focus, focused) =
            crate::motion::focus_ring::focus_reveal(&button_id, true, window, cx);
        let hover = self.hover_progress(&state.row_key, window, cx).max(focused);
        Some(
            div()
                .relative()
                .flex_none()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .opacity(1. - hover)
                        .child(Icon::new(IconName::Pin).text_color(cx.theme().muted_foreground)),
                )
                .child(
                    Button::new(button_id)
                        .track_focus(&focus)
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::PinOff).text_color(cx.theme().muted_foreground))
                        .aria_label(label.clone())
                        .tooltip(label)
                        // As the settle button does, over the row's pointer.
                        .cursor_default()
                        .absolute()
                        .top_0()
                        .left_0()
                        // Opacity keeps the button a tab stop, as the settle button does.
                        .opacity(hover)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.unpin_thread(&thread, window, cx);
                        })),
                ),
        )
    }

    /// The dot leading a thread row's title: unread over the status's colour,
    /// breathing while only background work keeps the thread going; none
    /// while the row is renamed. A change cross-fades.
    pub(super) fn status_dot(
        name: &str,
        show_unread: bool,
        renaming: bool,
        status: Option<(gpui::Hsla, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let dot = (!renaming)
            .then(|| show_unread.then(|| (cx.theme().primary, false)).or(status))
            .flatten();
        faded_status(
            format!("status-dot-{name}"),
            dot,
            window,
            cx,
            |(color, breathes), _, _| row::lamp(*color, *breathes),
        )
    }

    /// The word naming a thread row's status, its tooltip what keeps the
    /// thread going or why it failed. A change cross-fades, and a status
    /// that ends fades out in its place.
    pub(super) fn status_word(
        state: &ThreadRowState,
        status: Option<&row::RowStatus>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let name = &state.name;
        let word = faded_status(
            format!("status-word-{name}"),
            status.map(|status| (SharedString::from(status.verb()), status.color(cx))),
            window,
            cx,
            |(label, color), _, cx| {
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_secondary(false, cx)
                    .text_color(*color)
                    .child(crate::widgets::fade_text(label.clone()))
                    .into_any_element()
            },
        )?;
        let keeps = keeps_going(state);
        Some(
            div()
                .id(SharedString::from(format!("thread-status-{name}")))
                .when_some(keeps, |word, keeps| {
                    word.with_tooltip(move |_, _| Tooltip::new(keeps.clone()))
                })
                .when(state.failed && !state.waiting(), |word| {
                    word.with_tooltip(|_, _| Tooltip::new(crate::tr!("sidebar.failed_tooltip")))
                })
                .child(word),
        )
    }

    pub(super) fn thread_context_menu(
        &self,
        row: crate::material::Clickable<gpui::Div>,
        meta: &SessionMeta,
        state: &ThreadRowState,
        running: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let section = tcode_core::thread_sort::thread_section(meta);
        let settled = section == ThreadSection::Settled;
        let Some(store) = self.store_of(&state.thread.host) else {
            return row.into_any_element();
        };
        let share = self.share_target(&state.thread.host, meta.project_id.as_deref(), cx);
        let full = store.read(cx).may_operate();
        let view_only = store.read(cx).view_only();
        let sidebar = cx.entity().downgrade();
        let thread = state.thread.clone();
        let can_fork = state.menu_can_fork;
        let is_worktree = state.is_worktree;
        let title_generating = state.title_generating;
        let settle_blocked = state.settle_blocked;
        let auto_settle_enabled = state.auto_settle_enabled;
        let follow_up = meta.follow_up.is_some();
        let parked = meta.parked_at.is_some();
        let watching = state.wakes.notify > 0;
        let hover = self.hover.clone();
        let row_key = state.row_key.clone();
        let draft = state.draft;
        let menu = row
            .context_menu(move |menu, window, cx| {
                let id = thread.clone();
                if draft {
                    let open_beside = crate::shell::pane_controls(window, cx)
                        .map(|panes| panes.split_right || crate::shell::shown_in_pane(&id, cx));
                    return menu
                        .when_some(open_beside, |menu, enabled| {
                            menu.menu_with_enable(
                                crate::tr!("panes.open_beside").into_owned(),
                                Box::new(crate::panes::OpenInNewPane(id.clone())),
                                enabled,
                            )
                        })
                        .when(full, |menu| {
                            menu.menu_element(Box::new(ThreadDelete(id.clone())), |_, cx| {
                                div().text_color(cx.theme().danger).child(
                                    crate::widgets::fade_text(crate::tr!("sidebar.ctx_delete")),
                                )
                            })
                        });
                }
                // A machine that lets this device only view keeps what reads
                // the thread, and nothing that changes it.
                if view_only {
                    let open_beside = crate::shell::pane_controls(window, cx)
                        .map(|panes| panes.split_right || crate::shell::shown_in_pane(&id, cx));
                    return menu
                        .when_some(open_beside, |menu, enabled| {
                            menu.menu_with_enable(
                                crate::tr!("panes.open_beside").into_owned(),
                                Box::new(crate::panes::OpenInNewPane(id.clone())),
                                enabled,
                            )
                            .separator()
                        })
                        .menu(
                            crate::tr!("sidebar.ctx_copy_path").into_owned(),
                            Box::new(ThreadCopyPath(id.clone())),
                        )
                        .menu(
                            crate::tr!("sidebar.ctx_copy_id").into_owned(),
                            Box::new(ThreadCopyId(id.clone())),
                        )
                        .separator()
                        .menu(
                            crate::tr!("sidebar.ctx_export_jsonl").into_owned(),
                            Box::new(ThreadExportJsonl(id.clone())),
                        )
                        .menu(
                            crate::tr!("sidebar.ctx_export_markdown").into_owned(),
                            Box::new(ThreadExportMarkdown(id)),
                        );
                }
                // Move up / Move down stay within the section the list shows.
                let moves = sidebar
                    .upgrade()
                    .filter(|_| !settled && full)
                    .and_then(|sidebar| sidebar.read(cx).move_bounds(&id, cx));
                let open_beside = crate::shell::pane_controls(window, cx)
                    .map(|panes| panes.split_right || crate::shell::shown_in_pane(&thread, cx));
                menu.when_some(open_beside, |menu, enabled| {
                    menu.menu_with_enable(
                        crate::tr!("panes.open_beside").into_owned(),
                        Box::new(crate::panes::OpenInNewPane(id.clone())),
                        enabled,
                    )
                    .separator()
                })
                .menu(
                    crate::tr!("sidebar.ctx_rename").into_owned(),
                    Box::new(ThreadRename(id.clone())),
                )
                .when(title_generating, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.ctx_stop_title").into_owned(),
                        Box::new(ThreadStopTitle(id.clone())),
                    )
                })
                .when(!title_generating, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.ctx_regenerate_title").into_owned(),
                        Box::new(ThreadRegenerateTitle(id.clone())),
                    )
                })
                .when(can_fork, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.ctx_fork").into_owned(),
                        Box::new(ThreadFork(id.clone())),
                    )
                })
                .when(is_worktree, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.ctx_merge_worktree").into_owned(),
                        Box::new(ThreadMergeWorktree(id.clone())),
                    )
                })
                .when(!is_worktree, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.ctx_move_thread").into_owned(),
                        Box::new(ThreadMoveToProject(id.clone())),
                    )
                })
                .menu(
                    crate::tr!("pull_requests.link_menu").into_owned(),
                    Box::new(ThreadLinkPullRequest(id.clone())),
                )
                .menu(
                    crate::tr!("sidebar.ctx_mark_unread").into_owned(),
                    Box::new(ThreadMarkUnread(id.clone())),
                )
                .separator()
                .menu(
                    if section == ThreadSection::Pinned {
                        crate::tr!("sidebar.ctx_unpin").into_owned()
                    } else {
                        crate::tr!("sidebar.ctx_pin").into_owned()
                    },
                    if section == ThreadSection::Pinned {
                        Box::new(ThreadUnpin(id.clone())) as Box<dyn Action>
                    } else {
                        Box::new(ThreadPin(id.clone()))
                    },
                )
                .menu_with_enable(
                    if settled {
                        crate::tr!("sidebar.ctx_unsettle").into_owned()
                    } else {
                        crate::tr!("sidebar.ctx_settle").into_owned()
                    },
                    if settled {
                        Box::new(ThreadMakeActive(id.clone())) as Box<dyn Action>
                    } else {
                        Box::new(ThreadSettle(id.clone()))
                    },
                    settled || !settle_blocked,
                )
                .when_some(moves, |menu, (up, down)| {
                    menu.menu_with_enable(
                        crate::tr!("sidebar.ctx_move_up").into_owned(),
                        Box::new(ThreadMove(id.clone(), false)),
                        up,
                    )
                    .menu_with_enable(
                        crate::tr!("sidebar.ctx_move_down").into_owned(),
                        Box::new(ThreadMove(id.clone(), true)),
                        down,
                    )
                })
                .when(compact, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.ctx_arrange").into_owned(),
                        Box::new(ThreadArrange(id.clone())),
                    )
                })
                .separator()
                .menu(
                    if follow_up {
                        crate::tr!("control.follow_up.remove")
                    } else {
                        crate::tr!("control.follow_up.add")
                    }
                    .into_owned(),
                    Box::new(ThreadFollowUp(id.clone(), !follow_up)),
                )
                .menu(
                    if parked {
                        crate::tr!("control.follow_up.unpark")
                    } else {
                        crate::tr!("control.follow_up.park")
                    }
                    .into_owned(),
                    Box::new(ThreadPark(id.clone(), !parked)),
                )
                .when(watching, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.ctx_stop_watches").into_owned(),
                        Box::new(ThreadStopWatches(id.clone())),
                    )
                })
                .separator()
                .label(crate::tr!("sidebar.ctx_auto_settle"))
                .menu_with_check(
                    crate::tr!("sidebar.ctx_auto_settle_enabled"),
                    auto_settle_enabled,
                    Box::new(ThreadAutoSettle(id.clone(), true)),
                )
                .menu_with_check(
                    crate::tr!("sidebar.ctx_auto_settle_disabled"),
                    !auto_settle_enabled,
                    Box::new(ThreadAutoSettle(id.clone(), false)),
                )
                .separator()
                .menu(
                    crate::tr!("sidebar.ctx_copy_path").into_owned(),
                    Box::new(ThreadCopyPath(id.clone())),
                )
                .menu(
                    crate::tr!("sidebar.ctx_copy_id").into_owned(),
                    Box::new(ThreadCopyId(id.clone())),
                )
                .separator()
                .menu(
                    crate::tr!("sidebar.ctx_export_jsonl").into_owned(),
                    Box::new(ThreadExportJsonl(id.clone())),
                )
                .menu(
                    crate::tr!("sidebar.ctx_export_markdown").into_owned(),
                    Box::new(ThreadExportMarkdown(id.clone())),
                )
                .separator()
                .menu_with_enable(
                    crate::tr!("sidebar.archive").into_owned(),
                    Box::new(ThreadArchive(id.clone())),
                    !running,
                )
                .when(full, |menu| {
                    menu.menu_element(Box::new(ThreadDelete(id.clone())), |_, cx| {
                        div()
                            .text_color(cx.theme().danger)
                            .child(crate::widgets::fade_text(crate::tr!("sidebar.ctx_delete")))
                    })
                })
                .when_some(share.as_ref(), |menu, share| {
                    spaces::share_items(menu, share, true, cx)
                })
            })
            .touch(compact);
        // A compact row reveals nothing.
        if compact {
            menu.into_any_element()
        } else {
            menu.reveals(&hover, row_key).into_any_element()
        }
    }

    pub(super) fn render_thread(
        &self,
        listed: &ListedThread,
        flags: &HashMap<String, ThreadFlags>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(store) = self.store_of(&listed.host) else {
            return div().into_any_element();
        };
        let meta = &listed.meta;
        let working = flags.get(&meta.id).is_some_and(|flags| flags.working);
        let state = self.thread_row_state(
            store.read(cx),
            listed,
            flags,
            format!("thread-{}", listed.name),
        );
        let light = self.shown.light(&state.thread);
        let name = state.name.clone();
        let is_worktree = state.is_worktree;
        let show_unread = state.show_unread;
        let status = row::RowStatus::of(&state, working);

        let row = self
            .thread_clickable_row(
                selectable_row(h_flex(), light == RowLight::Current, false, cx),
                meta,
                &state,
                light,
                false,
                Vec::new(),
                window,
                cx,
            )
            .pl(px(THREAD_INDENT))
            .children(Self::status_dot(
                &name,
                show_unread,
                state.renaming.is_some(),
                status
                    .as_ref()
                    .map(|status| (status.color(cx), status.breathes())),
                window,
                cx,
            ));

        // Row body: rename input, or the worktree glyph, the title and the
        // status word beside it, which gives up its width before the title.
        let row = if state.renaming.is_some() {
            row.child(self.thread_title_or_input(meta, &state, window, cx))
        } else {
            row.when(is_worktree, |row| {
                row.child(
                    Icon::empty()
                        .path("icons/git-branch.svg")
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        self.thread_title_or_input(meta, &state, window, cx)
                            .flex_initial(),
                    )
                    .children(
                        Self::status_word(&state, status.as_ref(), window, cx).map(|word| {
                            word.flex_shrink(1000.)
                                .min_w_0()
                                .overflow_hidden()
                                .pl(ROW_GAP)
                        }),
                    ),
            )
            .children(Self::follow_up_glyph(meta, &name, window, cx))
            .children(self.pin_glyph(meta, &state, window, cx))
            .children(Self::pr_badge(
                &store,
                meta,
                &name,
                state.renaming.is_none(),
                ROW_GAP,
                "pr-badge-row",
                false,
                window,
                cx,
            ))
            .children(self.pane_glyph(&state, window, cx))
            // The time narrows away while the thread works, giving the
            // title its width gradually, and widens back after.
            .when_some(
                crate::motion::shown(
                    SharedString::from(format!("thread-time-{name}")),
                    (!working).then_some(()),
                    SHOW,
                    window,
                    cx,
                ),
                |row, shown| {
                    let slot = self.render_flat_thread_right_slot(
                        meta,
                        &state,
                        project_view_timestamp(meta),
                        !state.settle_blocked,
                        window,
                        cx,
                    );
                    row.child(shown.fold_across_gap(
                        ROW_GAP,
                        div().flex_none().text_secondary(false, cx).child(slot),
                    ))
                },
            )
        };

        self.thread_context_menu(row, meta, &state, working, false, cx)
    }

    /// `timestamp` is the time the row shows: the one its list sorts it by.
    /// A settled row offers to make it active under the pointer, an active
    /// one to settle it while it `offers_settle`, and a draft to delete it.
    pub(super) fn render_flat_thread_right_slot(
        &self,
        meta: &SessionMeta,
        state: &ThreadRowState,
        timestamp: u64,
        offers_settle: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let action_id = state.thread.clone();
        let row_key = state.row_key.as_str();
        let settled = meta.is_settled();
        let ago = self
            .store_of(&state.thread.host)
            .map(|store| humanize_ago(store.read(cx).host_now_secs(cx).saturating_sub(timestamp)));
        let discards = state.draft_kept;
        // A machine that lets this device only view it settles and discards nothing.
        let view_only = self
            .store_of(&state.thread.host)
            .is_some_and(|store| store.read(cx).view_only());
        let action_on_hover =
            !view_only && (discards || (!state.draft && (settled || offers_settle)));
        let label = if discards {
            crate::tr!("sidebar.delete_draft_tooltip")
        } else if settled {
            crate::tr!("sidebar.unsettle_tooltip")
        } else {
            crate::tr!("sidebar.settle_tooltip")
        };
        let button_id = gpui::ElementId::from(SharedString::from(format!(
            "settle-flat-thread-{}",
            state.name
        )));
        let (focus, focused) =
            crate::motion::focus_ring::focus_reveal(&button_id, true, window, cx);
        let hover = if action_on_hover {
            self.hover_progress(row_key, window, cx).max(focused)
        } else {
            0.
        };
        div()
            .relative()
            .flex_none()
            .h(px(20.))
            .min_w(px(20.))
            .child(
                h_flex()
                    .h_full()
                    .items_center()
                    .whitespace_nowrap()
                    .opacity(1. - hover)
                    .children(ago.map(crate::widgets::fade_text)),
            )
            .when(action_on_hover, |slot| {
                slot.child(
                    Button::new(button_id)
                        .track_focus(&focus)
                        .ghost()
                        .xsmall()
                        .icon(
                            Icon::new(if discards {
                                IconName::Trash
                            } else if settled {
                                IconName::Undo2
                            } else {
                                IconName::CircleCheck
                            })
                            .text_color(cx.theme().muted_foreground),
                        )
                        .aria_label(label.clone())
                        .tooltip(label)
                        // The row's pointer covers its children; the button
                        // keeps Button's own cursor, the default.
                        .cursor_default()
                        .absolute()
                        .right_0()
                        .top_0()
                        // Opacity, not visibility, keeps the button a tab stop so
                        // keyboard focus can reveal it; a click's focus must not
                        // keep it over the time once the row moves.
                        .opacity(hover)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if discards {
                                this.discard_draft(&action_id, window, cx);
                            } else if settled {
                                this.on_make_active(
                                    &ThreadMakeActive(action_id.clone()),
                                    window,
                                    cx,
                                );
                            } else {
                                this.on_settle(&ThreadSettle(action_id.clone()), window, cx);
                            }
                        })),
                )
            })
    }

    pub(super) fn render_flat_thread(
        &self,
        listed: &ListedThread,
        flags: &HashMap<String, ThreadFlags>,
        project_name: Option<String>,
        host_name: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(store) = self.store_of(&listed.host) else {
            return div().into_any_element();
        };
        let meta = &listed.meta;
        let own_flags = flags.get(&meta.id);
        let working = own_flags.is_some_and(|flags| flags.working);
        let state = self.thread_row_state(
            store.read(cx),
            listed,
            flags,
            format!("flat-thread-{}", listed.name),
        );
        let light = self.shown.light(&state.thread);
        let shows = row::RowShows::of(
            store.read(cx),
            meta,
            &state,
            working,
            project_name.map(SharedString::from),
            host_name,
        );
        let time = self
            .render_flat_thread_right_slot(
                meta,
                &state,
                shown_timestamp(
                    meta,
                    own_flags.map(|flags| flags.standing).unwrap_or_default(),
                ),
                !state.settle_blocked,
                window,
                cx,
            )
            .into_any_element();
        let lines = self.row_lines(&store, meta, &state, &shows, time, false, window, cx);
        let row = self
            .thread_clickable_row(
                row::desktop_row_surface(light == RowLight::Current, cx),
                meta,
                &state,
                light,
                shows.pinned,
                shows.details(),
                window,
                cx,
            )
            .items_stretch()
            .justify_center()
            .child(lines);

        self.thread_context_menu(row, meta, &state, working, false, cx)
    }
}
