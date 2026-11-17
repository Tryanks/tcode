use super::*;

impl SessionsSidebar {
    pub(super) fn thread_lists(&mut self, cx: &App) -> ThreadLists {
        let cached = self.thread_lists.take().unwrap_or_default();
        let sectioned = self.sectioned();
        let down = |host: &SidebarHost| sectioned && host_down(host.store.read(cx));
        let hidden = |host: &SidebarHost| {
            host.spaces_observer
                .watched()
                .map(|spaces| spaces.read(cx).hidden_from_threads())
                .unwrap_or_default()
        };
        let current = |host: &SidebarHost, lists: &HostLists| {
            lists.host == host.key
                && lists.revision == host.store.read(cx).thread_list_revision()
                && lists.down == down(host)
                && *lists.hidden == hidden(host)
        };
        if cached.len() == self.hosts.len()
            && self
                .hosts
                .iter()
                .zip(cached.iter())
                .all(|(host, lists)| current(host, lists))
        {
            self.thread_lists = Some(cached.clone());
            return cached;
        }
        self.compact_model_dirty = true;
        let lists: ThreadLists = self
            .hosts
            .iter()
            .map(|host| {
                let kept = cached.iter().find(|lists| current(host, lists));
                kept.cloned().unwrap_or_else(|| {
                    let store = host.store.read(cx);
                    let revision = store.thread_list_revision();
                    if down(host) {
                        return HostLists {
                            host: host.key.clone(),
                            revision,
                            down: true,
                            groups: Rc::default(),
                            flat: Rc::default(),
                            flags: Rc::default(),
                            hidden: Rc::default(),
                        };
                    }
                    HostLists {
                        host: host.key.clone(),
                        revision,
                        down: false,
                        groups: Rc::new(store.grouped_sessions()),
                        flat: Rc::new(store.flat_sessions()),
                        flags: Rc::new(session_flags(store.sidebar_sessions(), store)),
                        hidden: Rc::new(hidden(host)),
                    }
                })
            })
            .collect();
        self.thread_lists = Some(lists.clone());
        lists
    }

    /// Whether the list is over several hosts, each in its own section.
    pub(super) fn sectioned(&self) -> bool {
        self.hosts.len() > 1
    }

    /// Whether any host the list shows holds its projects, not only a space.
    pub(super) fn full_scope(&self, cx: &App) -> bool {
        self.hosts
            .iter()
            .any(|host| host.store.read(cx).scope().is_full())
    }

    /// The store of `host`, while the sidebar lists it.
    pub(super) fn store_of(&self, host: &HostKey) -> Option<Entity<WorkspaceStore>> {
        self.hosts
            .iter()
            .find(|listed| listed.key == *host)
            .map(|listed| listed.store.clone())
    }

    /// `id` of `host`, as [`named`] names it in this sidebar.
    pub(super) fn name<'a>(&self, host: &HostKey, id: &'a str) -> Cow<'a, str> {
        named(self.hosts.len(), host, id)
    }

    /// The thread [`Self::name`] names `name`.
    pub(super) fn thread_named(&self, name: &str) -> Option<ThreadKey> {
        self.hosts.iter().find_map(|host| {
            let session_id = name.strip_prefix(self.name(&host.key, "").as_ref())?;
            Some(ThreadKey {
                host: host.key.clone(),
                session_id: session_id.to_owned(),
            })
        })
    }

    pub(super) fn shelf_name(&self, shelf: &Shelf) -> String {
        match shelf {
            Shelf::Recent => "recent".into(),
            Shelf::Project(project) => self.name(&project.host, &project.project_id).into_owned(),
        }
    }

    pub(super) fn listed(&self, (host, meta): Listed) -> ListedThread {
        ListedThread {
            host: host.clone(),
            meta: meta.clone(),
            name: self.name(host, &meta.id).into_owned(),
        }
    }

    pub(super) fn is_selected(&self, host: &HostKey, id: &str) -> bool {
        self.last_selected
            .as_ref()
            .is_some_and(|selected| selected.host == *host && selected.session_id == id)
    }

    /// The rows `scope` shows, as a drag over them would leave them, each
    /// section of every host's merged by time ([`merged`]).
    pub(super) fn thread_rows<'a>(
        &self,
        lists: &'a [HostLists],
        scope: Option<&ProjectKey>,
        shelf: &Shelf,
    ) -> ThreadRows<'a> {
        let hosts = lists
            .iter()
            .filter_map(|lists| {
                let project = host_scope(scope, &lists.host)?;
                let threads = project_threads(&lists.flat, project, &lists.hidden);
                let host = self.arranged(&lists.host, scope, threads);
                Some(tagged_sections(&lists.host, host))
            })
            .collect();
        let ThreadSections {
            pinned,
            active,
            mut settled,
        } = merged(hosts);
        let settled_count = settled.len();
        self.limit_settled_rows(shelf, &mut settled);
        ThreadRows {
            pinned,
            active,
            settled_hidden_count: settled_count - settled.len(),
            settled,
            settled_count,
        }
    }

    /// The project view lists each project's active threads in the order the
    /// store gives them, then its Settled shelf; Pinned belongs to the threads
    /// view.
    pub(super) fn grouped_rows(&self, lists: &[HostLists], cx: &App) -> Vec<(GroupedListRow, f32)> {
        let mut rows = self.grouped_rows_folding(lists, cx);
        rows.retain(|(_, factor)| *factor >= 0.);
        rows
    }

    /// [`Self::grouped_rows`] with the rows a moving disclosure leaves out of
    /// the list, at a negative factor. Each host's projects follow the last
    /// host's, under its heading when there are several.
    pub(super) fn grouped_rows_folding(
        &self,
        lists: &[HostLists],
        cx: &App,
    ) -> Vec<(GroupedListRow, f32)> {
        let mut rows = Vec::new();
        let sectioned = lists.len() > 1;
        for (host, of) in lists.iter().enumerate() {
            let Some(store) = self.store_of(&of.host) else {
                continue;
            };
            let store = store.read(cx);
            let host_collapsed = DevicePreferences::host_collapsed(&of.host, cx);
            let host_key = fold_key(Fold::Host, of.host.name());
            if sectioned {
                rows.push((
                    GroupedListRow::Host {
                        host,
                        collapsed: host_collapsed,
                    },
                    1.,
                ));
                if !self.fold_shown(&host_key, !host_collapsed) {
                    continue;
                }
            }
            let host_start = rows.len();
            for (group, project) in of.groups.iter().enumerate() {
                let project_id = &project.project.id;
                let name = self.name(&of.host, project_id);
                let key = ProjectKey {
                    host: of.host.clone(),
                    project_id: project_id.clone(),
                };
                let collapsed = store.is_project_collapsed(project_id);
                rows.push((
                    GroupedListRow::Project {
                        host,
                        group,
                        collapsed,
                    },
                    1.,
                ));
                let project_key = fold_key(Fold::Project, &name);
                if !self.fold_shown(&project_key, !collapsed) {
                    continue;
                }
                let start = rows.len();
                let (active, settled) = project_group_threads(&project.sessions);
                let expanded = self.expanded_groups.contains(&key);
                let more_key = fold_key(Fold::More, &name);
                let shown = if self.fold_shown(&more_key, expanded) {
                    active.len()
                } else {
                    THREADS_COLLAPSED_LIMIT
                };
                let more = self.fold_progress(&more_key, expanded);
                let extra = active.len().saturating_sub(THREADS_COLLAPSED_LIMIT);
                rows.extend(active.iter().take(shown).enumerate().map(|(index, meta)| {
                    let factor = match index.checked_sub(THREADS_COLLAPSED_LIMIT) {
                        Some(index) => fold_factor(more, index, extra),
                        None => 1.,
                    };
                    let row = self.listed((&of.host, meta));
                    (GroupedListRow::Thread(Box::new(row)), factor)
                }));
                if let Some(label) = thread_list_toggle_label(active.len(), expanded) {
                    rows.push((
                        GroupedListRow::ToggleThreads {
                            host,
                            group,
                            expanded,
                            label: label.into(),
                        },
                        1.,
                    ));
                }
                if !settled.is_empty() {
                    let count = settled.len();
                    rows.push((GroupedListRow::Settled { host, group, count }, 1.));
                    let shelf = Shelf::Project(key);
                    let mut listed: Vec<Listed> = tagged(&of.host, settled).collect();
                    self.limit_settled_rows(&shelf, &mut listed);
                    let hidden = count - listed.len();
                    let more = self.settled_more_count(&shelf, hidden);
                    let (factors, more_factor) =
                        self.shelf_factors(&shelf, &listed, more.is_some());
                    rows.extend(listed.into_iter().zip(factors).map(|(row, factor)| {
                        (GroupedListRow::Thread(Box::new(self.listed(row))), factor)
                    }));
                    if let Some(count) = more {
                        rows.push((
                            GroupedListRow::SettledMore { host, group, count },
                            more_factor,
                        ));
                    }
                }
                let progress = self.fold_progress(&project_key, !collapsed);
                let count = rows.len() - start;
                for (index, (_, factor)) in rows[start..].iter_mut().enumerate() {
                    *factor = nested_fold(fold_factor(progress, index, count), *factor);
                }
            }
            if sectioned {
                let progress = self.fold_progress(&host_key, !host_collapsed);
                let count = rows.len() - host_start;
                for (index, (_, factor)) in rows[host_start..].iter_mut().enumerate() {
                    *factor = nested_fold(fold_factor(progress, index, count), *factor);
                }
            }
        }
        rows
    }

    /// Use the same filters, disclosures and ordering as the rendered list,
    /// including rows outside the scroll viewport.
    pub(super) fn navigation_threads(&mut self, cx: &mut Context<Self>) -> Vec<ThreadKey> {
        if self.compact(cx) {
            return self
                .compact_model(None, cx)
                .rows
                .iter()
                .filter_map(|row| match row {
                    CompactListRow::Thread(row) => Some(row.state.thread.clone()),
                    _ => None,
                })
                .collect();
        }

        let lists = self.thread_lists(cx);
        let mut threads = Vec::new();
        match DevicePreferences::sidebar_layout(cx) {
            SidebarLayout::Flat => {
                let rows = self.thread_rows(&lists, self.project_filter.as_ref(), &Shelf::Recent);
                threads.extend(
                    rows.pinned
                        .into_iter()
                        .chain(rows.active)
                        .chain(rows.settled)
                        .map(|(host, meta)| ThreadKey {
                            host: host.clone(),
                            session_id: meta.id.clone(),
                        }),
                );
            }
            SidebarLayout::Grouped => {
                threads.extend(
                    self.grouped_rows(&lists, cx)
                        .into_iter()
                        .filter_map(|(row, _)| match row {
                            GroupedListRow::Thread(row) => Some(row.key()),
                            _ => None,
                        }),
                );
            }
        }
        threads
    }

    pub(crate) fn navigate_thread(
        &mut self,
        action: &crate::shortcut::NavigateThread,
        current: Option<&ThreadKey>,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::shortcut::NavigateThread;

        let threads = self.navigation_threads(cx);
        if threads.is_empty() {
            return false;
        }
        let active = current.and_then(|thread| {
            let store = self.store_of(&thread.host)?;
            Some(ThreadKey {
                host: thread.host.clone(),
                session_id: store.read(cx).roster_row(&thread.session_id),
            })
        });
        let current = threads.iter().position(|id| Some(id) == active.as_ref());
        let index = match action {
            NavigateThread::Index(index) => *index,
            NavigateThread::Next => current.map_or(0, |index| (index + 1) % threads.len()),
            NavigateThread::Previous => current.map_or(threads.len() - 1, |index| {
                (index + threads.len() - 1) % threads.len()
            }),
        };
        if let Some(thread) = threads.get(index)
            && let Some(store) = self.store_of(&thread.host)
        {
            self.compact_model_dirty = true;
            store.update(cx, |store, cx| {
                store.show_thread(thread.session_id.clone(), cx);
            });
            cx.notify();
            return true;
        }
        false
    }

    pub(super) fn compact(&self, cx: &gpui::App) -> bool {
        self.window_state.read(cx).compact
    }

    /// How far `group`'s hover controls have faded in.
    pub(super) fn hover_progress(&self, group: &str, window: &mut Window, cx: &mut App) -> f32 {
        self.hover.reveal(group, window, cx)
    }

    /// Whether `key`'s rows are in the list: open, or still shutting.
    pub(super) fn fold_shown(&self, key: &str, open: bool) -> bool {
        open || self.fold_progress(key, open) > 0.
    }

    pub(super) fn fold_progress(&self, key: &str, open: bool) -> f32 {
        self.folds
            .get(key)
            .copied()
            .unwrap_or(if open { 1. } else { 0. })
    }

    /// Sample every disclosure the list on screen has, so one that opens or
    /// shuts moves from where it was. Keyed by the list's scope, so a scope
    /// change that resets them does not animate.
    pub(super) fn sample_folds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = self.compact(cx);
        let layout = DevicePreferences::sidebar_layout(cx);
        let lists = self.thread_lists(cx);
        let mut targets = Vec::new();
        match layout {
            SidebarLayout::Flat => targets.push((
                fold_key(Fold::Shelf, "recent"),
                self.expanded_settled.contains(&Shelf::Recent),
            )),
            SidebarLayout::Grouped => {
                let groups: usize = lists.iter().map(|lists| lists.groups.len()).sum();
                let sectioned = self.sectioned();
                let headers = !compact || groups > 1 || sectioned;
                for lists in lists.iter() {
                    let Some(store) = self.store_of(&lists.host) else {
                        continue;
                    };
                    if sectioned {
                        targets.push((
                            fold_key(Fold::Host, lists.host.name()),
                            !DevicePreferences::host_collapsed(&lists.host, cx),
                        ));
                    }
                    for group in lists.groups.iter() {
                        let id = group.project.id.as_str();
                        let name = self.name(&lists.host, id);
                        let project = ProjectKey {
                            host: lists.host.clone(),
                            project_id: id.to_owned(),
                        };
                        let collapsed = store.read(cx).is_project_collapsed(id)
                            && (headers || group.project.quick_chat);
                        targets.push((fold_key(Fold::Project, &name), !collapsed));
                        targets.push((
                            fold_key(Fold::More, &name),
                            self.expanded_groups.contains(&project),
                        ));
                        targets.push((
                            fold_key(Fold::Shelf, &name),
                            self.expanded_settled.contains(&Shelf::Project(project)),
                        ));
                    }
                }
            }
        }
        let filter = self
            .project_filter
            .as_ref()
            .map(|project| self.name(&project.host, &project.project_id));
        let scope = format!("{layout:?}-{filter:?}-{compact}");
        let mut folds = HashMap::with_capacity(targets.len());
        let mut moving = false;
        for (key, open) in targets {
            let target = if open { 1. } else { 0. };
            let progress = transition(
                SharedString::from(format!("fold-{scope}-{key}")),
                target,
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            );
            moving |= progress != target || self.folds.get(&key) != Some(&progress);
            folds.insert(key, progress);
        }
        self.folds = folds;
        // A new scope puts its disclosures where they rest at once: the list
        // it brings is replaced as a whole, its rows coming and going as the
        // list's own (`RowPresence`), not as a disclosure's.
        let rescoped = self.fold_scope.as_deref() != Some(scope.as_str());
        self.folding = moving && !rescoped;
        self.fold_scope = Some(scope);
        if moving {
            self.compact_model_dirty = true;
        }
    }

    /// A list row inside folds, at `factor` of its height: the row is drawn
    /// through [`crate::motion::fold`], clipped and fading with the height
    /// it has. Every row of a list goes through here, so a row keeps its
    /// element tree as its folds move. `id` names the row, not its index.
    pub(super) fn fold_row(
        &mut self,
        id: impl Into<gpui::ElementId>,
        factor: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
        render: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) -> gpui::AnyElement,
    ) -> gpui::AnyElement {
        let row = render(self, window, cx);
        crate::motion::fold(id, factor.clamp(0., 1.), row)
    }

    /// Whether the threads are loading on every host: once one host has
    /// them, the list shows them. A host whose link is down in a list over
    /// several is not loading: it shows its heading.
    pub(super) fn threads_loading(&self, cx: &App) -> bool {
        let sectioned = self.sectioned();
        self.hosts.iter().all(|host| {
            let store = host.store.read(cx);
            store.threads_loading() && !(sectioned && host_down(store))
        })
    }

    /// The list's body: while threads load, the skeleton (or the rows still
    /// waiting for a connection); once loaded, `loaded`, fading in under the
    /// skeleton as it fades out.
    pub(super) fn loading_body(
        &mut self,
        loaded: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) -> gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let loading = self.threads_loading(cx);
        // The host in this process answers within moments of the window
        // opening: a skeleton would be gone almost as soon as drawn, so the
        // list stays as it was until that host answers, then shows at once.
        if loading
            && self
                .hosts
                .iter()
                .any(|host| !host.store.read(cx).is_remote())
        {
            return div().flex_1().into_any_element();
        }
        let skeleton = loading
            && self
                .hosts
                .iter()
                .all(|host| host.store.read(cx).pending_sessions().is_empty());
        let leaving = crate::motion::shown(
            "sidebar-loading-skeleton",
            skeleton.then_some(()),
            SHOW,
            window,
            cx,
        );
        let arriving = transition(
            ("sidebar-loaded", "opacity"),
            if loading { 0. } else { 1. },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        if skeleton {
            return crate::material::loading_skeleton(cx);
        }
        if loading {
            return self.pending_thread_rows(window, cx);
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .relative()
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .opacity(arriving)
                    .child(loaded(self, window, cx)),
            )
            .when_some(leaving, |body, leaving| {
                body.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(leaving.fade(crate::material::loading_skeleton(cx))),
                )
            })
            .into_any_element()
    }

    /// Track the pointer over `group`'s row, a positioned row that takes
    /// clicks, for [`Self::hover_progress`].
    pub(super) fn track_hover<R: gpui::ParentElement>(&self, row: R, group: &str) -> R {
        row.child(self.hover.hitbox(group.to_owned()))
    }
    pub(crate) fn holds_overlay(&self, cx: &App) -> bool {
        self.renaming.is_some()
            || (self.drag.is_some() && cx.has_active_drag())
            || [&self.flat_list_state, &self.grouped_list_state]
                .iter()
                .any(|list| list.is_scrollbar_dragging())
    }
}
