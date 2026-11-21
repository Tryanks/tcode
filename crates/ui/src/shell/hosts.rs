use super::*;

// ---------------------------------------------------------------------------
// Attachment lifecycle
// ---------------------------------------------------------------------------

impl AppShell {
    pub(super) fn navigation_snapshot(&self, cx: &App) -> NavigationSnapshot {
        let thread = self.page_thread(cx);
        let panel = self
            .pending_navigation_restore
            .as_ref()
            .map(|pending| pending.panel)
            .unwrap_or_else(|| {
                if self.panel_shows_terminal {
                    NavigationPanel::Terminal
                } else {
                    match thread.as_ref().and_then(|thread| {
                        let store = self.link_of(&thread.host)?.store.read(cx);
                        Some(store.panel_state_for(Some(&thread.session_id)).right_tab)
                    }) {
                        Some(RightTab::Plan) => NavigationPanel::Plan,
                        Some(RightTab::Agents) => NavigationPanel::Agents,
                        Some(RightTab::PullRequests) => NavigationPanel::PullRequests,
                        Some(RightTab::Preview) => NavigationPanel::Preview,
                        _ => NavigationPanel::Diff,
                    }
                }
            });
        NavigationSnapshot {
            history: self.window_state.read(cx).history().to_vec(),
            thread,
            panel,
        }
    }

    pub(super) fn schedule_navigation_save(&mut self, cx: &mut Context<Self>) {
        if self.navigation.is_none() {
            return;
        }
        if let Some(pending) = &self.pending_navigation_restore
            && self.page_thread(cx).as_ref() == Some(&pending.thread)
        {
            return;
        }
        let snapshot = self.navigation_snapshot(cx);
        let navigation = self.navigation.as_mut().expect("enabled above");
        if navigation.scheduled.as_ref() == Some(&snapshot) {
            return;
        }
        navigation.scheduled = Some(snapshot.clone());
        navigation.task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            if let Ok(snapshot) = serde_json::to_value(snapshot) {
                cx.update(|cx| DevicePreferences::save_navigation(snapshot, cx));
            }
        }));
    }

    pub(super) fn reconcile_navigation_restore(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = &self.pending_navigation_restore else {
            return;
        };
        let Some(store) = self
            .link_of(&pending.thread.host)
            .map(|link| link.store.clone())
        else {
            self.pending_navigation_restore = None;
            return;
        };
        let shown = self
            .page_thread(cx)
            .filter(|shown| *shown == pending.thread);
        if !store.read(cx).index_hydrated() {
            if shown.is_none() {
                self.pending_navigation_restore = None;
            }
            return;
        }
        let session_id = &pending.thread.session_id;
        if !store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .any(|meta| meta.id == *session_id && meta.archived_at.is_none())
        {
            let pending = self
                .pending_navigation_restore
                .take()
                .expect("checked above");
            if shown.is_some() {
                store.update(cx, |store, cx| {
                    store.close_thread(&pending.thread.session_id, cx)
                });
            }
            self.window_state
                .update(cx, |state, cx| state.drop_thread_pages(cx));
            return;
        }
        if shown.is_none() {
            // A user selection made while syncing supersedes restoration.
            self.pending_navigation_restore = None;
            return;
        }
        if store.read(cx).chat_loading_for(Some(session_id), cx) {
            return;
        }
        let pending = self
            .pending_navigation_restore
            .take()
            .expect("checked above");
        if pending.panel_open {
            self.show_panel(pending.panel.id(), cx);
        } else {
            let tab = match pending.panel {
                NavigationPanel::Plan => RightTab::Plan,
                NavigationPanel::Agents => RightTab::Agents,
                NavigationPanel::PullRequests => RightTab::PullRequests,
                NavigationPanel::Preview => RightTab::Preview,
                _ => RightTab::Diff,
            };
            store.update(cx, |store, cx| {
                store.set_right_tab_for(Some(&pending.thread.session_id), tab, cx)
            });
        }
        self.schedule_navigation_save(cx);
    }

    /// The active scope of this window's workspace.
    pub(super) fn active_scope(&self, cx: &App) -> tcode_client::host::WorkspaceScope {
        let local = self.setup.local.is_some();
        let client = &self.client;
        let saved = client.load_hosts().unwrap_or_default();
        let preference = DevicePreferences::workspace(cx);
        let last_host_id = client.last_host_id();
        let local_machine_id = client.local_machine_id().ok().flatten();
        let local_group = client.local_machine_group().ok().flatten();
        crate::workspace::active_scope(&crate::workspace::Device {
            saved: &saved,
            preference: preference.as_ref(),
            last_host_id: last_host_id.as_deref(),
            local,
            local_machine_id: local_machine_id.as_deref(),
            local_group_id: local_group.as_ref().map(|(id, _)| id.as_str()),
            fixed_machine: client.fixed_machine(),
        })
    }

    /// The display name of the active machine group, when the window is open
    /// on a machine group.
    pub(super) fn active_group_name(&self, cx: &App) -> Option<String> {
        let tcode_client::host::WorkspaceScope::Group { group_id } = self.active_scope(cx) else {
            return None;
        };
        let saved = self.client.load_hosts().unwrap_or_default();
        let local_group = self.client.local_machine_group().ok().flatten();
        crate::workspace::group_name(&group_id, &saved, local_group.as_ref()).map(str::to_owned)
    }

    /// The hosts this device's active scope holds, in order.
    pub(super) fn members(&self, cx: &App) -> Vec<AttachmentTarget> {
        let local = self.setup.local.is_some();
        let client = &self.client;
        // Saved machines that cannot be read are none; opening the window
        // said why.
        let saved = client.load_hosts().unwrap_or_default();
        let preference = DevicePreferences::workspace(cx);
        let last_host_id = client.last_host_id();
        // Unreadable, it was reported on opening; the group's roster is not
        // saved meanwhile, so no saved machine is this one.
        let local_machine_id = client.local_machine_id().ok().flatten();
        let local_group = client.local_machine_group().ok().flatten();
        crate::workspace::members(&crate::workspace::Device {
            saved: &saved,
            preference: preference.as_ref(),
            last_host_id: last_host_id.as_deref(),
            local,
            local_machine_id: local_machine_id.as_deref(),
            local_group_id: local_group.as_ref().map(|(id, _)| id.as_str()),
            fixed_machine: client.fixed_machine(),
        })
    }

    /// Mutate the device's persisted workspace preference.
    pub(super) fn arrange_workspace(
        &self,
        change: impl FnOnce(&mut tcode_client::host::WorkspacePreference),
        cx: &mut App,
    ) {
        // Arranged from saved machines that cannot be read, the workspace
        // would lose every one of them.
        if self.client.load_hosts().is_err() {
            return;
        }
        DevicePreferences::arrange_workspace(cx, |workspace| {
            let arranged = workspace.get_or_insert_with(Default::default);
            change(arranged);
        });
    }

    /// Open this window's active scope at launch. `initial`, when supplied,
    /// focuses that host if it belongs to the active scope, or opens it alone
    /// if the launch explicitly targeted a host outside it (for example
    /// `--connect`, a pending outbox, or a space); that host is then the
    /// scope the device keeps, so the one left behind does not open beside
    /// it when its roster next changes. The host in this process is left out
    /// unless `with_local`.
    pub(super) fn open_workspace(
        &mut self,
        initial: Option<AttachmentTarget>,
        with_local: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scope = self.active_scope(cx);
        let mut targets = self.members(cx);
        if !with_local {
            targets.retain(|target| *target != AttachmentTarget::Local);
        }
        let mut outside = false;
        let persisted_scope = if let Some(initial) = &initial {
            if targets.iter().any(|target| same_target(target, initial))
                || (*initial == AttachmentTarget::Local && !with_local)
            {
                scope
            } else {
                outside = true;
                targets = vec![initial.clone()];
                match initial {
                    AttachmentTarget::Local => tcode_client::host::WorkspaceScope::Local,
                    AttachmentTarget::Remote(host) => tcode_client::host::WorkspaceScope::Host {
                        host_id: host.host_id.clone(),
                    },
                }
            }
        } else {
            scope
        };
        let kept =
            DevicePreferences::workspace(cx).is_some_and(|workspace| workspace.scope.is_some());
        if outside || (!kept && with_local) {
            self.arrange_workspace(
                |workspace| crate::workspace::set_scope(workspace, persisted_scope),
                cx,
            );
        }
        for target in targets {
            if self.link_of(&target.key()).is_none() {
                self.open_target(target, window, cx);
            }
        }
        self.order_links(cx);
        self.sync_hosts(cx);
        let opened = |key: &HostKey| self.link_of(key).is_some();
        let host = initial
            .map(|initial| initial.key())
            .filter(opened)
            .or_else(|| Some(HostKey::Local).filter(opened))
            .or_else(|| self.links.first().map(Link::key));
        let Some(host) = host else {
            self.window_state
                .update(cx, |state, cx| state.leave_workspace(cx));
            cx.notify();
            return;
        };
        self.show_column(host.clone(), window, cx);
        self.window_state
            .update(cx, |state, cx| state.enter_workspace(cx));
        self.show_pending_session(&host, cx);
        cx.notify();
    }

    /// Reconcile the window when `hosts.json` (or this machine's group)
    /// changes. Only a machine group view opens newly learned members of that
    /// same group; a single-host view is never mixed with unrelated group
    /// members. On a desktop viewing this machine alone, newly joining a group
    /// upgrades the view to that group once.
    pub(super) fn on_saved_hosts_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current_local_group = self
            .client
            .local_machine_group()
            .ok()
            .flatten()
            .map(|(id, _)| id);
        if current_local_group != self.known_local_group {
            let previous =
                std::mem::replace(&mut self.known_local_group, current_local_group.clone());
            match (previous, current_local_group, self.active_scope(cx)) {
                (_, Some(group_id), tcode_client::host::WorkspaceScope::Local) => {
                    self.arrange_workspace(
                        |workspace| {
                            crate::workspace::set_scope(
                                workspace,
                                tcode_client::host::WorkspaceScope::Group {
                                    group_id: group_id.clone(),
                                },
                            );
                        },
                        cx,
                    );
                }
                (
                    Some(left_group),
                    None,
                    tcode_client::host::WorkspaceScope::Group { group_id },
                ) if left_group == group_id => {
                    self.arrange_workspace(
                        |workspace| {
                            crate::workspace::set_scope(
                                workspace,
                                tcode_client::host::WorkspaceScope::Local,
                            );
                        },
                        cx,
                    );
                    let targets = self.members(cx);
                    self.apply_scope_targets(targets, Some(HostKey::Local), false, window, cx);
                    self.hosts.update(cx, |_, cx| cx.notify());
                    return;
                }
                _ => {}
            }
        }
        if matches!(
            self.active_scope(cx),
            tcode_client::host::WorkspaceScope::Group { .. }
        ) {
            let new: Vec<_> = self
                .members(cx)
                .into_iter()
                // The host in this process is opened at launch, or asked for
                // again after it failed to start.
                .filter(|target| *target != AttachmentTarget::Local)
                .filter(|target| self.link_of(&target.key()).is_none())
                .collect();
            if !new.is_empty() {
                for target in new {
                    self.open_target(target, window, cx);
                }
                match self.links.first().map(Link::key) {
                    Some(host) if self.column.is_none() => self.enter_first_host(host, window, cx),
                    _ => {
                        self.order_links(cx);
                        self.sync_hosts(cx);
                    }
                }
            }
        }
        self.hosts.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// Switch this window to `target` alone (this machine, a standalone
    /// machine, a group member viewed on its own, or a space). A link the
    /// machine refused is rebuilt; an already-working link to `target` is kept
    /// while every other link closes.
    pub(super) fn open_host(
        &mut self,
        target: AttachmentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = target.key();
        // Already showing only this exact target and it works: return to threads.
        if let [current] = self.links.as_slice()
            && same_target(&current.target, &target)
            && !refused(&current.store, cx)
        {
            let scope = match &target {
                AttachmentTarget::Local => tcode_client::host::WorkspaceScope::Local,
                AttachmentTarget::Remote(saved) => tcode_client::host::WorkspaceScope::Host {
                    host_id: saved.host_id.clone(),
                },
            };
            self.arrange_workspace(
                |workspace| crate::workspace::set_scope(workspace, scope),
                cx,
            );
            self.show_host_section(&host, cx);
            return;
        }
        // Never drop a working link for a local host that cannot start.
        let prestarted_local = if target == AttachmentTarget::Local {
            if self.link_of(&HostKey::Local).is_some() {
                None
            } else {
                let Ok(local) = self.start_local(&target, cx) else {
                    return;
                };
                local
            }
        } else {
            None
        };
        if target == AttachmentTarget::Local && self.setup.local.is_none() {
            log::error!("this client cannot reach {target:?}");
            return;
        }
        let scope = match &target {
            AttachmentTarget::Local => tcode_client::host::WorkspaceScope::Local,
            AttachmentTarget::Remote(saved) => tcode_client::host::WorkspaceScope::Host {
                host_id: saved.host_id.clone(),
            },
        };
        self.arrange_workspace(
            |workspace| crate::workspace::set_scope(workspace, scope),
            cx,
        );
        self.apply_scope_targets_with_local(
            vec![target],
            prestarted_local,
            Some(host),
            true,
            window,
            cx,
        );
    }

    /// Switch this window to the multi-host view of `group_id`. If every saved
    /// member of that group was previously hidden, bring them all back so the
    /// group is never empty.
    pub(super) fn open_group(
        &mut self,
        group_id: String,
        focus: Option<HostKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.arrange_workspace(
            |workspace| {
                crate::workspace::set_scope(
                    workspace,
                    tcode_client::host::WorkspaceScope::Group {
                        group_id: group_id.clone(),
                    },
                );
            },
            cx,
        );
        let mut targets = self.members(cx);
        if targets.is_empty() {
            let saved = self.client.load_hosts().unwrap_or_default();
            let member_ids: Vec<String> = saved
                .iter()
                .filter(|host| {
                    host.space_id.is_none() && host.group_id.as_deref() == Some(group_id.as_str())
                })
                .map(|host| host.host_id.clone())
                .collect();
            if !member_ids.is_empty() {
                self.arrange_workspace(
                    |workspace| {
                        crate::workspace::clear_left_for(
                            workspace,
                            member_ids.iter().map(String::as_str),
                        );
                    },
                    cx,
                );
                targets = self.members(cx);
            }
        }
        self.apply_scope_targets(targets, focus, true, window, cx);
    }

    /// Switch this window to a newly paired machine: its machine group when it
    /// belongs to one, otherwise that machine alone.
    pub(super) fn open_paired(
        &mut self,
        host: tcode_client::pairing::PairedHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if host.space_id.is_none()
            && let Some(group_id) = host.group_id.clone()
        {
            let host_id = host.host_id.clone();
            self.arrange_workspace(
                |workspace| crate::workspace::include_in_group(workspace, &host_id),
                cx,
            );
            self.open_group(group_id, Some(HostKey::Remote(host_id)), window, cx);
            return;
        }
        self.open_host(AttachmentTarget::Remote(Box::new(host)), window, cx);
    }

    /// Show a previously hidden member again inside its group's multi-host view.
    pub(super) fn include_in_group(
        &mut self,
        group_id: String,
        host_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.arrange_workspace(
            |workspace| crate::workspace::include_in_group(workspace, &host_id),
            cx,
        );
        self.open_group(group_id, Some(HostKey::Remote(host_id)), window, cx);
    }

    /// Reconcile the window's open links with `targets`: when none of the
    /// currently open links survive, retire the old workspace as a whole; when
    /// at least one surviving non-refused link stays open, close only the links
    /// leaving and open the ones arriving without disturbing surviving links.
    fn apply_scope_targets(
        &mut self,
        targets: Vec<AttachmentTarget>,
        focus: Option<HostKey>,
        reveal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_scope_targets_with_local(targets, None, focus, reveal, window, cx);
    }

    fn apply_scope_targets_with_local(
        &mut self,
        targets: Vec<AttachmentTarget>,
        mut prestarted_local: Option<tcode_client::host::Transport>,
        focus: Option<HostKey>,
        reveal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if targets.is_empty() {
            let links = std::mem::take(&mut self.links);
            self.retire(links, cx);
            self.sync_hosts(cx);
            self.window_state
                .update(cx, |state, cx| state.leave_workspace(cx));
            cx.notify();
            return;
        }
        let keeps_any = self.links.iter().any(|link| {
            !refused(&link.store, cx)
                && targets
                    .iter()
                    .any(|target| same_target(&link.target, target))
        });
        if !keeps_any {
            self.pending_navigation_restore = None;
            self.blur_navigation(window, cx);
            let links = std::mem::take(&mut self.links);
            self.retire(links, cx);
            for target in targets {
                if target == AttachmentTarget::Local
                    && let Some(local) = prestarted_local.take()
                {
                    self.open_link(target, Some(local), window, cx);
                } else {
                    self.open_target(target, window, cx);
                }
            }
            self.order_links(cx);
            self.sync_hosts(cx);
            let opened = |key: &HostKey| self.link_of(key).is_some();
            let host = focus
                .filter(opened)
                .or_else(|| Some(HostKey::Local).filter(opened))
                .or_else(|| self.links.first().map(Link::key));
            let Some(host) = host else {
                self.window_state
                    .update(cx, |state, cx| state.leave_workspace(cx));
                cx.notify();
                return;
            };
            self.show_column(host.clone(), window, cx);
            self.window_state
                .update(cx, |state, cx| state.enter_workspace(cx));
            self.show_pending_session(&host, cx);
            cx.notify();
            return;
        }
        // Keep surviving links open; close links that left or were refused.
        let to_close: Vec<HostKey> = self
            .links
            .iter()
            .filter(|link| {
                refused(&link.store, cx)
                    || !targets
                        .iter()
                        .any(|target| same_target(&link.target, target))
            })
            .map(Link::key)
            .collect();
        for key in to_close {
            self.close_link(&key, window, cx);
        }
        for target in targets {
            if self.link_of(&target.key()).is_none() {
                if target == AttachmentTarget::Local
                    && let Some(local) = prestarted_local.take()
                {
                    self.open_link(target, Some(local), window, cx);
                } else {
                    self.open_target(target, window, cx);
                }
            }
        }
        if self.column.is_none() {
            if let Some(host) = focus
                .clone()
                .filter(|key| self.link_of(key).is_some())
                .or_else(|| self.links.first().map(Link::key))
            {
                self.blur_navigation(window, cx);
                self.enter_first_host(host, window, cx);
            }
        } else {
            self.order_links(cx);
            self.sync_hosts(cx);
        }
        if reveal
            && let Some(host) = focus
                .filter(|key| self.link_of(key).is_some())
                .or_else(|| self.links.first().map(Link::key))
        {
            self.show_host_section(&host, cx);
        }
        cx.notify();
    }

    /// The window had no host and has `host`: its workspace replaces what
    /// the window showed, as a whole.
    pub(super) fn enter_first_host(
        &mut self,
        host: HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.retire(Vec::new(), cx);
        self.order_links(cx);
        self.sync_hosts(cx);
        self.show_column(host.clone(), window, cx);
        self.window_state
            .update(cx, |state, cx| state.enter_workspace(cx));
        self.show_pending_session(&host, cx);
    }

    /// The window has gone: close its link to every host. The hosts, this
    /// machine's included, keep running.
    pub fn close_links(&mut self, cx: &mut Context<Self>) {
        let links = std::mem::take(&mut self.links);
        self.retire(links, cx);
        self.sync_hosts(cx);
        cx.notify();
    }

    /// Take `host` out of the current view: in a machine group, only its link
    /// closes and it stays hidden from that group; on a single remote host or
    /// space, a desktop returns to this machine and a client without a local
    /// host empties the window.
    pub(super) fn leave_host(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The host in this process is never disconnected.
        if *host == HostKey::Local || self.link_of(host).is_none() {
            return;
        }
        let in_group = matches!(
            self.active_scope(cx),
            tcode_client::host::WorkspaceScope::Group { .. }
        );
        if in_group {
            self.arrange_workspace(
                |workspace| crate::workspace::leave_group_member(workspace, host),
                cx,
            );
            let last = self.links.len() == 1;
            self.close_link(host, window, cx);
            if last {
                self.arrange_workspace(
                    |workspace| {
                        crate::workspace::set_scope(
                            workspace,
                            tcode_client::host::WorkspaceScope::Empty,
                        );
                    },
                    cx,
                );
            }
            return;
        }
        if self.setup.local.is_some() {
            self.open_host(AttachmentTarget::Local, window, cx);
            return;
        }
        self.arrange_workspace(
            |workspace| {
                crate::workspace::set_scope(workspace, tcode_client::host::WorkspaceScope::Empty);
            },
            cx,
        );
        self.close_link(host, window, cx);
    }

    /// Move `host` one place earlier, or later, among the current group's hosts.
    pub(super) fn move_host(&mut self, host: &HostKey, earlier: bool, cx: &mut Context<Self>) {
        let members: Vec<HostKey> = self.members(cx).iter().map(AttachmentTarget::key).collect();
        self.arrange_workspace(
            |workspace| crate::workspace::shift(workspace, &members, host, earlier),
            cx,
        );
        self.order_links(cx);
        self.sync_hosts(cx);
        cx.notify();
    }

    /// Close the link to `host` and the panes showing its threads, and
    /// nothing else: every other host keeps its link, store and panes. The
    /// window's last pane stays, waiting on the workspace's first host left;
    /// the last link closing leaves the window with none.
    pub(super) fn close_link(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(at) = self.links.iter().position(|link| link.key() == *host) else {
            return;
        };
        if self.links.len() == 1 {
            let links = std::mem::take(&mut self.links);
            self.retire(links, cx);
            self.sync_hosts(cx);
            self.window_state
                .update(cx, |state, cx| state.leave_workspace(cx));
            cx.notify();
            return;
        }
        let mut link = self.links.remove(at);
        link.close(cx);
        self.close_panes_of(&link.store, window, cx);
        self.sync_hosts(cx);
        cx.notify();
    }

    /// The panes showing threads of `store`'s host, whose link closed, go,
    /// with the compact pages over them. The last pane of the window stays
    /// in place, waiting on the workspace's first host, which offers its
    /// draft.
    pub(super) fn close_panes_of(
        &mut self,
        store: &Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(column) = &self.column else {
            return;
        };
        if self
            .pending_navigation_restore
            .as_ref()
            .is_some_and(|pending| pending.thread.host == store.read(cx).host_key())
        {
            self.pending_navigation_restore = None;
        }
        let on_page = column.page().read(cx).store() == store;
        if on_page {
            self.drop_leads(0, cx);
            self.release_leaving(cx);
        }
        let Some(column) = &self.column else {
            return;
        };
        let panes = column.panes(cx);
        let mut remaining = panes.len();
        let gone: Vec<_> = panes
            .into_iter()
            .filter(|pane| pane.read(cx).store() == store)
            .collect();
        for pane in &gone {
            pane.update(cx, |pane, cx| pane.release_preview(cx));
            if remaining > 1 {
                if let Some(column) = self.column.as_mut()
                    && column.zoom.pane() == Some(pane)
                {
                    column.zoom.zoom_out();
                }
                self.glide_out(pane, cx);
                remaining -= 1;
                continue;
            }
            let Some(link) = self.links.first() else {
                return;
            };
            let (store, preview) = (link.store.clone(), link.preview.clone());
            self.rehost(pane, &store, &preview, window, cx);
            self.fill_empty_column(&store, cx);
        }
        self.replace_current_pane(cx);
        if on_page {
            self.window_state
                .update(cx, |state, cx| state.drop_thread_pages(cx));
        }
    }

    /// The links in the workspace's order; a host opened outside it last.
    pub(super) fn order_links(&mut self, cx: &App) {
        let order: Vec<HostKey> = self.members(cx).iter().map(AttachmentTarget::key).collect();
        self.links.sort_by_key(|link| {
            order
                .iter()
                .position(|member| *member == link.key())
                .unwrap_or(order.len())
        });
    }

    /// Back to the threads, with `host`'s section open among them.
    pub(super) fn show_host_section(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        self.window_state
            .update(cx, |state, cx| state.leave_hosts(cx));
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.reveal_host(host, cx));
    }

    /// Close `links`, the column, if any, and the sidebar over them, which
    /// a new one replaces, and, in the wide layout, keep their views on
    /// screen, inert, for the next workspace to rise over.
    /// The column's preview pages go at once: a native view draws over
    /// everything, the workspace rising over it included.
    pub(super) fn retire(&mut self, mut links: Vec<Link>, cx: &mut Context<Self>) {
        for link in &mut links {
            link.close(cx);
        }
        let column = self.column.take().map(|mut column| {
            column._subscriptions.clear();
            for pane in column.all_panes(cx) {
                pane.update(cx, |pane, cx| pane.release_preview(cx));
            }
            column
        });
        let window_state = self.window_state.clone();
        let sidebar = std::mem::replace(
            &mut self.sidebar,
            cx.new(|cx| SessionsSidebar::new(Vec::new(), window_state, cx)),
        );
        if !self.compact(cx) {
            self.departed.push(Departed {
                key: self.scene_key,
                column,
                links,
                sidebar,
                route: self.window_state.read(cx).route(),
                destination: self.destination(cx),
            });
        }
        self.scene_key += 1;
    }

    /// Hosts, the sidebar and the palette follow the window onto the hosts
    /// it is linked to now.
    pub(super) fn sync_hosts(&self, cx: &mut Context<Self>) {
        let stores: Vec<_> = self.links.iter().map(|link| link.store.clone()).collect();
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_hosts(stores.clone(), cx));
        self.palette
            .update(cx, |palette, cx| palette.set_stores(stores.clone(), cx));
        for link in &self.links {
            link.settings_page
                .update(cx, |page, cx| page.set_hosts(stores.clone(), cx));
        }
        self.hosts
            .update(cx, |hosts, cx| hosts.set_stores(stores, cx));
    }

    /// The transport of the host in this process when `target` is that host.
    /// `Err` where it did not start: Hosts says why.
    pub(super) fn start_local(
        &mut self,
        target: &AttachmentTarget,
        cx: &mut Context<Self>,
    ) -> Result<Option<tcode_client::host::Transport>, ()> {
        let (AttachmentTarget::Local, Some(start)) = (target, &self.setup.local) else {
            return Ok(None);
        };
        match start(cx) {
            Ok(transport) => {
                self.hosts
                    .update(cx, |hosts, cx| hosts.set_local_failure(None, cx));
                Ok(Some(transport))
            }
            Err(reason) => {
                log::error!("the local host is unavailable: {reason}");
                self.hosts
                    .update(cx, |hosts, cx| hosts.set_local_failure(Some(reason), cx));
                cx.notify();
                Err(())
            }
        }
    }

    /// Link this window to `target` beside the hosts it is linked to.
    pub(super) fn open_target(
        &mut self,
        target: AttachmentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<HostKey> {
        let local = self.start_local(&target, cx).ok()?;
        self.open_link(target, local, window, cx)
    }

    /// A message still on its way to a thread of `host` shows that thread.
    pub(super) fn show_pending_session(&self, host: &HostKey, cx: &mut Context<Self>) {
        let Some(store) = self.link_of(host).map(|link| link.store.clone()) else {
            return;
        };
        let pending = store
            .read(cx)
            .pending_sessions()
            .first()
            .map(|(id, _)| id.clone());
        if let Some(id) = pending {
            store.update(cx, |store, cx| store.show_thread(id, cx));
        }
    }

    /// Link this window to `target`, over `local` when it is the host in
    /// this process. `None` where this client cannot reach it.
    pub(super) fn open_link(
        &mut self,
        target: AttachmentTarget,
        local: Option<tcode_client::host::Transport>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<HostKey> {
        let name = match &target {
            AttachmentTarget::Local => crate::tr!("hosts.this_computer").into_owned(),
            AttachmentTarget::Remote(host) => host.name.clone(),
        };
        let open = match Attachment::open(target, local, self.client.clone(), cx) {
            Ok(open) => open?,
            Err(error) => {
                window.push_notification(
                    Notification::error(
                        crate::tr!("errors.outbox_unreadable", host = name, error = error)
                            .into_owned(),
                    ),
                    cx,
                );
                return None;
            }
        };
        let host = open.target.key();
        let store = open.store.clone();
        let preview =
            cx.new(|cx| PreviewPanel::new(store.clone(), self.window_state.clone(), window, cx));
        let mut subscriptions = vec![
            cx.observe_in(&store, window, |this, store, window, cx| {
                this.host_changed(&store, window, cx)
            }),
            cx.subscribe_in(
                &store,
                window,
                |this, store, event: &RuntimeEvent, w, cx| {
                    this.present_app_event(store, event, w, cx);
                },
            ),
            cx.subscribe_in(&store, window, |this, store, shown: &ShowThread, w, cx| {
                this.show_thread(store, shown, w, cx);
            }),
            cx.subscribe_in(
                &store,
                window,
                |this, store, change: &StoreChange, window, cx| match change.topic {
                    TopicKind::Settings => Self::adopt_device_preferences(store, window, cx),
                    TopicKind::Index => this.fill_empty_column(store, cx),
                    _ => {}
                },
            ),
        ];
        subscriptions.extend(PreviewPanel::observe_native_fade(&preview, cx));
        // The host sends a client only the requests of threads it renders,
        // so only a client that can render pages has any to answer.
        #[cfg(preview_backend)]
        let preview_pump = {
            let requests = store.read(cx).remote_preview_requests();
            let preview = preview.clone();
            let store = store.downgrade();
            cx.spawn_in(window, async move |_, cx| {
                // The requests still waited for, closed when the host cancels one.
                let waiting =
                    std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::<
                        u64,
                        async_channel::Receiver<_>,
                    >::new()));
                while let Ok(envelope) = requests.recv().await {
                    let (request_id, session_id, request) = match envelope.event {
                        tcode_protocol::ServerEvent::PreviewRequest {
                            request_id,
                            session_id,
                            request,
                        } => (request_id, session_id, request),
                        tcode_protocol::ServerEvent::PreviewRequestCancelled { request_id } => {
                            if let Some(receiver) = waiting.borrow_mut().remove(&request_id) {
                                receiver.close();
                            }
                            continue;
                        }
                        _ => continue,
                    };
                    let (reply, receiver) = async_channel::bounded(1);
                    waiting.borrow_mut().insert(request_id, receiver.clone());
                    // A request sent before the thread's page moved away
                    // would otherwise open the page here again.
                    if store
                        .read_with(cx, |store, _| !store.renders_preview(&session_id))
                        .unwrap_or(true)
                    {
                        let _ = reply.try_send(Err(
                            "the preview moved to another client; call the tool again".into(),
                        ));
                    } else if preview
                        .update_in(cx, |panel, window, cx| {
                            panel.handle_op(session_id, request, reply, window, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                    // Each operation waits independently: a slow wait_for must
                    // not block a second client's navigation or a screenshot.
                    let store = store.clone();
                    let waiting = waiting.clone();
                    cx.spawn(async move |cx| {
                        let response = receiver.recv().await;
                        // Cancelled by the host, which waits for it no more.
                        if waiting.borrow_mut().remove(&request_id).is_none() {
                            return;
                        }
                        let response = response
                            .unwrap_or_else(|_| Err("preview panel dropped request".into()));
                        let _ =
                            store.update(cx, |store, _| store.preview_reply(request_id, response));
                    })
                    .detach();
                }
            })
        };
        self.links.push(Link {
            target: open.target.clone(),
            store: store.clone(),
            open: Some(open),
            preview,
            settings_page: cx
                .new(|cx| SettingsPage::new(store.clone(), self.window_state.clone(), window, cx)),
            stamped: false,
            connected: false,
            attention: Default::default(),
            #[cfg(preview_backend)]
            _tasks: vec![preview_pump],
            #[cfg(not(preview_backend))]
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        });
        self.mark_on_screen(&store, cx);
        Some(host)
    }

    /// A column of one pane waiting for a thread of `host`, which the window
    /// is linked to.
    pub(super) fn show_column(
        &mut self,
        host: HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(link) = self.link_of(&host) else {
            return;
        };
        let (store, preview) = (link.store.clone(), link.preview.clone());
        let sidebar_width = self.column.as_ref().map_or_else(
            || Rc::new(Cell::new(px(SIDEBAR_WIDTH))),
            |column| column.sidebar_width.clone(),
        );
        let thread_panel = self.new_pane(&store, &preview, window, cx);
        let shell = cx.entity().downgrade();
        let drop_landing = Rc::new(Cell::new(None));
        let drop_target: crate::panes::DropTarget = Rc::new({
            let drop_landing = drop_landing.clone();
            move |window, cx| {
                let target = shell.upgrade()?.read(cx).drop_target(window, cx);
                drop_landing.set(target);
                target
            }
        });
        let split_handle_held = crate::panes::HandleHeld::default();
        let even_split: crate::panes::EvenSplit = Rc::new({
            let shell = cx.entity().downgrade();
            move |axis, at, window, cx| {
                if let Some(shell) = shell.upgrade() {
                    shell.update(cx, |shell, cx| shell.even_split(axis, at, window, cx));
                }
            }
        });
        let thread_panes = cx.new(|cx| {
            let mut area = DockArea::new("thread-panes", None, window, cx).with_renderer(Rc::new(
                crate::panes::ThreadPanes::new(drop_target, split_handle_held.clone(), even_split),
            ));
            area.set_center(DockLayout::tabs().panel(thread_panel.clone()), window, cx);
            area
        });
        let sidebar_focus = cx.focus_handle();
        let mut subscriptions = vec![cx.subscribe_in(
            &thread_panes,
            window,
            |this, _, event: &DockEvent, window, cx| {
                if let DockEvent::DragDrop { item, target } = event
                    && let Some(thread) = crate::sidebar::thread_carried(item)
                {
                    this.drop_thread(thread.clone(), *target, window, cx);
                }
                // The threads list draws a map of the panes.
                if matches!(event, DockEvent::LayoutChanged) {
                    cx.notify();
                }
            },
        )];
        subscriptions.push(cx.observe(&self.sidebar, |this, _, cx| {
            if this
                .column
                .as_ref()
                .is_some_and(|column| column.sidebar_overlay_visible)
            {
                cx.notify();
            }
        }));
        subscriptions.push(cx.on_focus_out(&sidebar_focus, window, |_, _, _, cx| cx.notify()));
        self.column = Some(Column {
            thread_panes,
            glides: Default::default(),
            window_seen: None,
            zoom: Default::default(),
            layout_swap: Default::default(),
            routes: Default::default(),
            thread_body: Default::default(),
            thread_body_shown: None,
            leads: Vec::new(),
            leaving: Vec::new(),
            split_handle_held,
            drop_landing,
            held_split: None,
            current_pane: thread_panel,
            handed: HashMap::new(),
            split: cx.new(|_| ResizableState::default()),
            panel_glide: crate::motion::resize_glide::ResizeGlide::new(
                "workspace-panels",
                gpui::Axis::Horizontal,
            ),
            sidebar_width,
            sidebar_overlay_visible: false,
            sidebar_focus,
            sidebar_pointer_in_window: true,
            _subscriptions: subscriptions,
        });
        self.remember_host(cx);
    }

    /// The host a relaunch returns to is the current pane's, whichever
    /// host that pane came to show.
    pub(super) fn remember_host(&self, cx: &App) {
        let (client, Some(column)) = (&self.client, &self.column) else {
            return;
        };
        let host_id = match column.current_pane.read(cx).host(cx) {
            HostKey::Local => None,
            HostKey::Remote(host_id) => Some(host_id),
        };
        if client.last_host_id() != host_id {
            client.set_last_host_id(host_id.as_deref());
        }
    }

    /// The current pane left the column: the first pane is current.
    pub(super) fn replace_current_pane(&mut self, cx: &mut Context<Self>) {
        let Some(column) = self.column.as_mut() else {
            return;
        };
        let panes = column.panes(cx);
        if !panes.contains(&column.current_pane)
            && let Some(first) = panes.first()
        {
            column.current_pane = first.clone();
            self.remember_host(cx);
        }
    }

    /// A pane over `store`'s host, waiting for a thread.
    pub(super) fn new_pane(
        &self,
        store: &Entity<WorkspaceStore>,
        preview: &Entity<PreviewPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ThreadPanel> {
        let drawer = self.drawer_over(store, preview, None, cx);
        let chat =
            cx.new(|cx| ChatView::new(store.clone(), self.window_state.clone(), None, window, cx));
        let shell = cx.entity().downgrade();
        cx.new(|cx| {
            ThreadPanel::new(
                chat,
                drawer,
                move |pane, _, cx| {
                    _ = shell.update(cx, |shell, cx| shell.pane_entered(pane, cx));
                },
                window,
                cx,
            )
        })
    }

    /// A drawer over `store`'s host, for a pane showing `session_id`.
    pub(super) fn drawer_over(
        &self,
        store: &Entity<WorkspaceStore>,
        preview: &Entity<PreviewPanel>,
        session_id: Option<&str>,
        cx: &mut App,
    ) -> crate::panes::Drawer {
        crate::panes::Drawer::new(
            store.clone(),
            self.window_state.clone(),
            preview.clone(),
            session_id,
            cx,
        )
    }

    /// `pane` waits for a thread of `store`'s host in place of what it
    /// showed of another's.
    pub(super) fn rehost(
        &self,
        pane: &Entity<ThreadPanel>,
        store: &Entity<WorkspaceStore>,
        preview: &Entity<PreviewPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drawer = self.drawer_over(store, preview, None, cx);
        let chat =
            cx.new(|cx| ChatView::new(store.clone(), self.window_state.clone(), None, window, cx));
        pane.update(cx, |pane, cx| {
            pane.replace_chat(chat, None, cx);
            pane.set_drawer(drawer, cx);
        });
        self.remember_host(cx);
    }

    /// A pane whose thread `store` closed leaves the column, and the last
    /// one, on a compact window the page, takes that thread's parent, or
    /// else the host's draft. The panes of every other host stay as they
    /// are.
    pub(super) fn reconcile_panes(
        &mut self,
        store: &Entity<WorkspaceStore>,
        cx: &mut Context<Self>,
    ) {
        let compact = self.compact(cx);
        if compact {
            self.reconcile_leads(store, cx);
        }
        let Some(column) = &self.column else {
            return;
        };
        let panes = column.panes(cx);
        let current_pane = column.current_pane.clone();
        let mut closed: Vec<_> = panes
            .iter()
            .filter(|pane| {
                let pane = pane.read(cx);
                pane.store() == store
                    && pane
                        .session_id()
                        .is_some_and(|session_id| !store.read(cx).is_thread_open(session_id))
            })
            .cloned()
            .collect();
        // What stays is the pane in use, of those the store emptied.
        closed.sort_by_key(|pane| *pane == current_pane);
        let mut remaining = panes.len();
        for pane in closed {
            if remaining == 1 || (compact && pane == current_pane) {
                let left = pane.read(cx).session_id().map(str::to_owned);
                let follows = left.is_some_and(|left| {
                    store.update(cx, |store, cx| store.show_after_left(&left, cx))
                });
                if !follows {
                    self.drop_leads(0, cx);
                    pane.update(cx, |pane, cx| pane.set_session(None, cx));
                    store.update(cx, |store, cx| store.open_fallback_draft(cx));
                }
            } else {
                if let Some(column) = self.column.as_mut()
                    && column.zoom.pane() == Some(&pane)
                {
                    column.zoom.zoom_out();
                }
                self.glide_out(&pane, cx);
                remaining -= 1;
            }
        }
        self.replace_current_pane(cx);
    }

    /// The pointer pressed in `pane` or the keyboard entered it, its drawer
    /// included: it is the current pane.
    pub(super) fn pane_entered(&mut self, pane: Entity<ThreadPanel>, cx: &mut Context<Self>) {
        if self
            .column
            .as_ref()
            .is_none_or(|column| !column.panes(cx).contains(&pane))
        {
            return;
        }
        self.set_current_pane(&pane, cx);
        cx.notify();
    }

    /// `pane` is the current pane. Coming from another pane, the user has
    /// moved on.
    pub(super) fn set_current_pane(&mut self, pane: &Entity<ThreadPanel>, cx: &mut Context<Self>) {
        let Some(column) = self.column.as_mut() else {
            return;
        };
        if column.current_pane != *pane {
            column.current_pane = pane.clone();
            self.remember_host(cx);
            self.moved_on(cx);
        }
    }

    /// The user moved on, wherever in the window: a thread still in flight
    /// on any host, a draft asked for there included, lands nowhere.
    pub(super) fn moved_on(&self, cx: &mut Context<Self>) {
        for link in &self.links {
            link.store.update(cx, |store, _| store.selection_moved_on());
        }
    }

    /// The only rebind of a pane: it shows `thread`, open from here on, over
    /// its host in place of the host it showed, and the thread it showed
    /// closes unless a side card shows that too.
    pub(super) fn bind_pane(
        &mut self,
        pane: &Entity<ThreadPanel>,
        thread: &ThreadKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(column) = &self.column else {
            return;
        };
        let Some(link) = self.link_of(&thread.host) else {
            return;
        };
        let (store, preview) = (link.store.clone(), link.preview.clone());
        let panes = column.panes(cx);
        let session_id = thread.session_id.as_str();
        store.update(cx, |store, cx| {
            if !store.is_thread_open(session_id) {
                store.open_thread(session_id, cx);
                cx.notify();
            }
        });
        let left = pane.read(cx).thread(cx);
        if left.as_ref() == Some(thread) {
            return;
        }
        let left_store = pane.read(cx).store().clone();
        let rehosted = left_store != store;
        let held = rehosted && pane.focus_handle(cx).contains_focused(window, cx);
        if rehosted {
            self.rehost(pane, &store, &preview, window, cx);
        }
        pane.update(cx, |pane, cx| {
            pane.set_session(Some(session_id.to_owned()), cx)
        });
        // A pane that waited with the keyboard had no composer to give it to
        // until now, nor had one whose composer went with its host.
        if held || (left.is_none() && pane.focus_handle(cx).is_focused(window)) {
            pane.read(cx)
                .chat()
                .update(cx, |chat, cx| chat.focus_composer(window, cx));
        }
        if let Some(left) = left
            && left_store.read(cx).is_thread_open(&left.session_id)
            && !drawn_beside(&panes, &left, cx)
        {
            left_store.update(cx, |store, cx| store.close_thread(&left.session_id, cx));
        }
    }

    /// Where a thread shown without naming a pane goes: the pane showing it,
    /// or beside its lead; else the pane of `replacing`, a thread of the
    /// same host, the current pane, or the first pane.
    pub(super) fn place(
        &self,
        thread: &ThreadKey,
        replacing: Option<&str>,
        cx: &App,
    ) -> Option<Entity<ThreadPanel>> {
        let column = self.column.as_ref()?;
        let panes = column.panes(cx);
        self.pane_showing(thread, cx)
            .or_else(|| {
                panes
                    .iter()
                    .find(|pane| shows_beside(pane, thread, cx))
                    .cloned()
            })
            .or_else(|| {
                let replacing = ThreadKey {
                    host: thread.host.clone(),
                    session_id: replacing?.to_owned(),
                };
                self.pane_showing(&replacing, cx)
            })
            .or_else(|| {
                panes
                    .contains(&column.current_pane)
                    .then(|| column.current_pane.clone())
            })
            .or_else(|| panes.first().cloned())
    }

    /// A column showing nothing offers the current pane's host's draft once
    /// that host's index is in, instead of a dead end. No other host is
    /// asked: the window would show only one of their drafts.
    pub(super) fn fill_empty_column(
        &mut self,
        store: &Entity<WorkspaceStore>,
        cx: &mut Context<Self>,
    ) {
        let Some(column) = &self.column else {
            return;
        };
        if column.current_pane.read(cx).store() != store
            || !store.read(cx).index_hydrated()
            || column
                .panes(cx)
                .iter()
                .any(|pane| pane.read(cx).session_id().is_some())
        {
            return;
        }
        store.update(cx, |store, cx| store.open_fallback_draft(cx));
    }

    /// What a change of `store` changes for the window.
    pub(super) fn host_changed(
        &mut self,
        store: &Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = store.read(cx).host_key();
        self.reconcile_panes(store, cx);
        if let Some(link) = self.links.iter_mut().find(|link| link.key() == host)
            && link.attention.update(store.read(cx), cx)
        {
            crate::attention::request(window);
        }
        let connected = store.read(cx).connection_state().is_connected();
        if let Some(link) = self.link_of_mut(&host)
            && std::mem::replace(&mut link.connected, connected) != connected
            && connected
        {
            self.stamp_connected(&host, cx);
        }
        self.reconcile_navigation_restore(cx);
        self.schedule_navigation_save(cx);
        cx.notify();
    }

    /// Put `shown`, a thread of `store`'s host, in its pane, whichever hosts
    /// the other panes show.
    pub(super) fn show_thread(
        &mut self,
        store: &Entity<WorkspaceStore>,
        shown: &ShowThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.column.is_none() {
            return;
        }
        if shown.asked {
            self.moved_on(cx);
            // A thread asked for from the settings is what the window shows next.
            self.window_state
                .update(cx, |state, cx| state.close_settings(cx));
        }
        let thread = store.read(cx).thread_key(&shown.session_id);
        if self.compact(cx) {
            self.unwind_leads(&thread, cx);
        }
        // A thread that opens beside another takes a pane of its own to
        // that one's right; where the layout has no room for one, or the
        // other is not shown, it is placed as any thread is.
        let from = shown.beside.as_ref().and_then(|from| {
            let from = ThreadKey {
                host: thread.host.clone(),
                session_id: from.clone(),
            };
            self.pane_showing(&from, cx)
        });
        if let Some(from) = from
            && self.open_in_new_pane(
                thread.clone(),
                &from,
                &[gpui_base::Placement::Right],
                window,
                cx,
            )
        {
            return;
        }
        if let Some(pane) = self.place(&thread, shown.replacing.as_deref(), cx) {
            self.show_in_pane(&pane, &thread, shown.asked, window, cx);
        }
    }

    /// `pane` shows `thread` and is the current pane. A thread the user
    /// `asked` for takes the compact stack and the keyboard; one the
    /// workspace went to by itself leaves them, though a lone pane's composer
    /// takes a keyboard nothing holds.
    pub(super) fn show_in_pane(
        &mut self,
        pane: &Entity<ThreadPanel>,
        thread: &ThreadKey,
        asked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .pending_navigation_restore
            .as_ref()
            .is_some_and(|pending| pending.thread != *thread)
        {
            self.pending_navigation_restore = None;
        }
        let compact = self.compact(cx);
        let beside = shows_beside(pane, thread, cx);
        if !beside {
            self.bind_pane(pane, thread, window, cx);
        }
        let chat = pane.read(cx).chat();
        let Some(column) = self.column.as_mut() else {
            return;
        };
        if column.zoom.pane().is_some_and(|zoomed| zoomed != pane) {
            column.zoom.zoom_out();
        }
        self.set_current_pane(pane, cx);
        self.blur_navigation(window, cx);
        self.window_state
            .update(cx, |state, cx| state.leave_route_for_chat(cx));
        if asked && compact {
            self.go(Destination::Thread, cx);
        }
        let unheld = window.focused(cx).is_none()
            && self
                .column
                .as_ref()
                .is_some_and(|column| column.panes(cx).len() == 1);
        if asked || unheld {
            chat.update(cx, |chat, cx| {
                if beside {
                    chat.focus_side(window, cx);
                } else {
                    chat.focus_composer(window, cx);
                }
            });
        }
        // Showing another thread can leave the destination unchanged. Its
        // checkpoint must not depend on the host delivering a notification.
        self.schedule_navigation_save(cx);
        cx.notify();
    }

    /// Record on the saved record when this window actually reached the host,
    /// so its row in Hosts can say how recently it worked. Only a live
    /// connection counts; an attempt that never came up is not one.
    pub(super) fn stamp_connected(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        let Some(link) = self.link_of_mut(host) else {
            return;
        };
        let AttachmentTarget::Remote(host) = &link.target else {
            return;
        };
        if std::mem::replace(&mut link.stamped, true) {
            return;
        }
        // The timestamp mutation must share the transport's persistence
        // transaction so it cannot restore a pre-migration address.
        cx.global::<crate::remote::ClientAttachment>()
            .host()
            .stamp_connected(&host.host_id, crate::time::now_secs(cx));
    }

    /// The first full-scope host whose settings arrive gives each device
    /// preference still unset its value.
    pub(super) fn adopt_device_preferences(
        store: &Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = store.read(cx);
        if !store.scope().is_full() || !store.settings_hydrated() {
            return;
        }
        if DevicePreferences::adopt(&store.settings(), cx) {
            crate::settings::apply_locale(DevicePreferences::language(cx).as_deref());
            crate::theme::apply_theme(DevicePreferences::theme(cx), window, cx);
        }
    }
}
