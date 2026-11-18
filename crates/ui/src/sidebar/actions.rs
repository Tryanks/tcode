use super::*;

impl SessionsSidebar {
    /// Prompt for a directory, then create a project rooted there on `host`.
    pub(super) fn add_project(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(host) {
            crate::add_project_dialog::open(store, window, cx);
        }
    }

    pub(super) fn toggle_project(&mut self, project: &ProjectKey, cx: &mut Context<Self>) {
        let Some(store) = self.store_of(&project.host) else {
            return;
        };
        if !store.read(cx).is_project_collapsed(&project.project_id) {
            self.expanded_groups.remove(project);
        }
        store.update(cx, |store, cx| {
            store.toggle_project_collapsed(project.project_id.clone(), cx);
        });
        cx.notify();
    }

    pub(super) fn on_filter_project(
        &mut self,
        action: &FilterProject,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_filter = action.0.clone();
        self.expanded_settled.clear();
        self.settled_limits.clear();
        self.compact_model_dirty = true;
        self.window_state
            .update(cx, |state, cx| state.leave_route_for_chat(cx));
        cx.notify();
    }

    pub(super) fn on_add_project(
        &mut self,
        action: &AddProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_project(&action.0, window, cx);
    }

    pub(super) fn on_sort_projects(
        &mut self,
        action: &SortProjects,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0) {
            store.update(cx, |store, cx| store.cycle_project_sort(cx));
        }
    }

    pub(super) fn on_leave_host(
        &mut self,
        action: &LeaveHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = cx.global::<crate::remote::ClientAttachment>().requester();
        request(
            crate::remote::HostRequest::Leave(action.0.clone()),
            window,
            cx,
        );
    }

    pub(super) fn on_host_settings(
        &mut self,
        action: &HostSettings,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = action.0.clone();
        self.window_state
            .update(cx, |state, cx| state.open_settings_on(host, cx));
    }

    pub(super) fn on_manage_hosts(
        &mut self,
        _: &ManageHosts,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window_state
            .update(cx, |state, cx| state.go(Destination::Hosts, cx));
    }

    pub(super) fn toggle_host(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        let collapsed = DevicePreferences::host_collapsed(host, cx);
        DevicePreferences::set_host_collapsed(host, !collapsed, cx);
        self.compact_model_dirty = true;
        cx.notify();
    }

    /// Open `host`'s section, which the window just brought in.
    pub(crate) fn reveal_host(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        if DevicePreferences::host_collapsed(host, cx) {
            self.toggle_host(host, cx);
        }
    }

    pub(super) fn on_link_pull_request(
        &mut self,
        action: &ThreadLinkPullRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            crate::pull_requests::open_link_dialog(store, action.0.session_id.clone(), window, cx);
        }
    }

    /// `thread`'s title, as its host lists it.
    pub(super) fn thread_title(&self, thread: &ThreadKey, cx: &App) -> Option<String> {
        let store = self.store_of(&thread.host)?;
        store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .find(|meta| meta.id == thread.session_id)
            .map(|meta| meta.title.clone())
    }

    pub(super) fn on_rename(
        &mut self,
        action: &ThreadRename,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = action.0.clone();
        let title = self.thread_title(&thread, cx).unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            state.set_value(&title, window, cx);
            state.focus(window, cx);
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, _input, event, window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_rename(window, cx),
                InputEvent::Blur => this.cancel_rename(cx),
                _ => {}
            },
        );
        self.renaming = Some(RenameState {
            thread,
            input,
            _sub: sub,
        });
        cx.notify();
    }

    pub(super) fn on_regenerate_title(
        &mut self,
        action: &ThreadRegenerateTitle,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.regenerate_session_title(action.0.session_id.clone());
            });
        }
    }

    pub(super) fn on_stop_title(
        &mut self,
        action: &ThreadStopTitle,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.cancel_title_generation(action.0.session_id.clone());
            });
        }
    }

    pub(super) fn on_fork(
        &mut self,
        action: &ThreadFork,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&action.0.host) else {
            return;
        };
        let id = action.0.session_id.clone();
        crate::chat::confirm_fork(crate::chat::ForkOf::Thread, window, cx, move |cx| {
            store.update(cx, |store, cx| store.fork_thread(id.clone(), cx));
        });
    }

    pub(super) fn on_move_to_project(
        &mut self,
        action: &ThreadMoveToProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            crate::move_thread_dialog::open(store, action.0.session_id.clone(), window, cx);
        }
    }

    pub(super) fn on_merge_worktree(
        &mut self,
        action: &ThreadMergeWorktree,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _cx| {
                store.merge_worktree(action.0.session_id.clone());
            });
        }
    }

    pub(super) fn commit_rename(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.renaming.take() {
            let value = state.input.read(cx).value().to_string();
            if let Some(store) = self.store_of(&state.thread.host) {
                store.update(cx, |store, _cx| {
                    store.rename_session(state.thread.session_id, value);
                });
            }
            cx.notify();
        }
    }

    pub(super) fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn on_mark_unread(
        &mut self,
        action: &ThreadMarkUnread,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            let id = action.0.session_id.clone();
            store.update(cx, |store, cx| {
                store.mark_session_unread(id, cx);
            });
        }
    }

    pub(super) fn on_copy_path(
        &mut self,
        action: &ThreadCopyPath,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&action.0.host) else {
            return;
        };
        if let Some(meta) = store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .find(|m| m.id == action.0.session_id)
        {
            let path = meta.cwd.to_string_lossy().into_owned();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(path));
        }
    }

    pub(super) fn on_copy_id(
        &mut self,
        action: &ThreadCopyId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(action.0.session_id.clone()));
    }

    pub(super) fn prompt_export(
        &self,
        thread: &ThreadKey,
        format: ThreadExportFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&thread.host) else {
            return;
        };
        let Some(cwd) = store
            .read(cx)
            .thread_meta(&thread.session_id)
            .map(|meta| meta.cwd.clone())
        else {
            return;
        };
        crate::thread_export::prompt_thread_export(
            store,
            thread.session_id.clone(),
            cwd,
            format,
            window,
            cx,
        );
    }

    pub(super) fn on_export_jsonl(
        &mut self,
        action: &ThreadExportJsonl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt_export(&action.0, ThreadExportFormat::Jsonl, window, cx);
    }

    pub(super) fn on_export_markdown(
        &mut self,
        action: &ThreadExportMarkdown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt_export(&action.0, ThreadExportFormat::Markdown, window, cx);
    }

    pub(super) fn on_auto_settle(
        &mut self,
        action: &ThreadAutoSettle,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.set_auto_settle(action.0.session_id.clone(), action.1)
            });
        }
    }

    pub(super) fn on_follow_up(
        &mut self,
        action: &ThreadFollowUp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.set_follow_up(action.0.session_id.clone(), action.1)
            });
        }
    }

    pub(super) fn on_park(&mut self, action: &ThreadPark, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.set_parked(action.0.session_id.clone(), action.1)
            });
        }
    }

    pub(super) fn on_stop_watches(
        &mut self,
        action: &ThreadStopWatches,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                let session_id = action.0.session_id.clone();
                let watched = store.watched_threads(&session_id);
                store.stop_watching_threads(session_id, watched)
            });
        }
    }

    pub(super) fn on_settle(
        &mut self,
        action: &ThreadSettle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settle_thread(&action.0, window, cx);
    }

    /// Settle a thread; its undo also restores a pin it loses.
    pub(super) fn settle_thread(
        &mut self,
        thread: &ThreadKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&thread.host) else {
            return;
        };
        let id = thread.session_id.as_str();
        let pin = store
            .read(cx)
            .thread_meta(id)
            .filter(|meta| meta.pinned_at.is_some())
            .map(|meta| Command::PinSession {
                session_id: id.to_owned(),
                order_key: meta.pin_order.clone(),
            });
        let undo = std::iter::once(Command::UnsettleSession {
            session_id: id.to_owned(),
        })
        .chain(pin)
        .collect();
        self.perform_lifecycle(
            &thread.host,
            vec![Command::SettleSession {
                session_id: id.to_owned(),
            }],
            Some((UndoKind::Settle, undo)),
            window,
            cx,
        );
    }

    /// Send `commands` to `host` in order and stop at the first refusal;
    /// once every one is accepted, offer `undo` in the shared toast.
    pub(super) fn perform_lifecycle(
        &mut self,
        host: &HostKey,
        commands: Vec<Command>,
        undo: Option<(UndoKind, Vec<Command>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(host) else {
            return;
        };
        let reopen = match commands.first() {
            Some(Command::ArchiveSession { session_id })
                if crate::shell::pane_shows(&store.read(cx).thread_key(session_id), cx) =>
            {
                Some(session_id.clone())
            }
            _ => None,
        };
        let host = host.clone();
        let sessions: Vec<String> = commands
            .iter()
            .filter_map(|command| command.session_id().map(str::to_owned))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            for command in commands {
                let settling = matches!(command, Command::SettleSession { .. });
                let pinning = matches!(command, Command::PinSession { .. });
                let swiped = match &command {
                    Command::SettleSession { session_id }
                    | Command::UnsettleSession { session_id } => Some(ThreadKey {
                        host: host.clone(),
                        session_id: session_id.clone(),
                    }),
                    _ => None,
                };
                let Ok(request) =
                    cx.update(|_, cx| store.update(cx, |store, cx| store.command(command, cx)))
                else {
                    return;
                };
                if let Err(error) = request.await {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.lifecycle_answered(&host, &sessions, false, cx);
                        if pinning {
                            this.reveal_pinned = None;
                        }
                        if let Some(thread) = swiped {
                            this.row_swipe_refused(&thread);
                        }
                        cx.notify();
                        let message = if settling && error.code == "thread_busy" {
                            crate::tr!("sidebar.settle_refused").into_owned()
                        } else {
                            error.message
                        };
                        window.push_notification(Notification::warning(message), cx)
                    });
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.lifecycle_answered(&host, &sessions, true, cx)
            });
            if let Some((kind, commands)) = undo {
                let _ = this.update_in(cx, |this, window, cx| {
                    let store = store.read(cx);
                    let harnesses = store
                        .sidebar_sessions()
                        .iter()
                        .filter(|meta| sessions.contains(&meta.id))
                        .map(|meta| store.harness_name_of(meta))
                        .collect();
                    this.push_lifecycle_undo(
                        kind,
                        UndoEntry {
                            host,
                            commands,
                            reopen,
                        },
                        harnesses,
                        window,
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    pub(super) fn push_lifecycle_undo(
        &mut self,
        kind: UndoKind,
        entry: UndoEntry,
        harnesses: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let live = window.has_notification::<ThreadUndo>(cx);
        if !live
            || self
                .lifecycle_undo
                .as_ref()
                .is_none_or(|undo| undo.kind != kind)
        {
            self.lifecycle_undo = Some(LifecycleUndo {
                kind,
                entries: vec![],
                harnesses: vec![],
            });
        }
        let undo = self.lifecycle_undo.as_mut().unwrap();
        undo.entries.push(entry);
        for harness in harnesses {
            if !undo.harnesses.contains(&harness) {
                undo.harnesses.push(harness);
            }
        }
        let count = undo.entries.len();
        let named = if undo.harnesses.is_empty() {
            crate::tr!("providers.harness.unnamed").into_owned()
        } else {
            crate::i18n::join_list(&undo.harnesses)
        };
        let title = match (kind, count) {
            (UndoKind::DiscardDraft, 1) => crate::tr!("sidebar.undo_draft_deleted_one"),
            (UndoKind::DiscardDraft, _) => {
                crate::tr!("sidebar.undo_draft_deleted", count = count)
            }
            (UndoKind::Settle, 1) => crate::tr!("sidebar.undo_settled_one", harness = named),
            (UndoKind::Unpin, 1) => crate::tr!("sidebar.undo_unpinned_one"),
            (UndoKind::Archive, 1) => crate::tr!("sidebar.undo_archived_one"),
            (UndoKind::Settle, _) => {
                crate::tr!("sidebar.undo_settled", count = count, harnesses = named)
            }
            (UndoKind::Unpin, _) => crate::tr!("sidebar.undo_unpinned", count = count),
            (UndoKind::Archive, _) => crate::tr!("sidebar.undo_archived", count = count),
        }
        .into_owned();
        let weak = cx.entity().downgrade();
        window.push_notification(
            Notification::new()
                .id::<ThreadUndo>()
                .message(title)
                .action(move |_, window, cx| {
                    let weak = weak.clone();
                    let button = Button::new("undo-thread-lifecycle")
                        .small()
                        .label(crate::tr!("sidebar.undo"))
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| this.undo_lifecycle(window, cx));
                        });
                    // The phone pill has no keyboard to hint at.
                    if crate::window_seam::window_is_compact(window, cx) {
                        button.ghost()
                    } else {
                        button.outline().when_some(
                            crate::widgets::kbd::Kbd::binding_for_action(
                                &ThreadUndo,
                                Some("TcodeShell"),
                                window,
                            ),
                            |button, kbd| button.child(kbd),
                        )
                    }
                })
                .autohide(true),
            cx,
        );
    }

    pub(crate) fn undo_lifecycle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !window.has_notification::<ThreadUndo>(cx) {
            self.lifecycle_undo = None;
            return;
        }
        let Some(undo) = self.lifecycle_undo.take() else {
            return;
        };
        window.remove_notification::<ThreadUndo>(cx);
        let entries: Vec<_> = undo
            .entries
            .into_iter()
            .rev()
            .filter_map(|entry| Some((self.store_of(&entry.host)?, entry)))
            .collect();
        cx.spawn_in(window, async move |_, cx| {
            for (store, entry) in entries {
                for command in entry.commands {
                    let Ok(request) =
                        cx.update(|_, cx| store.update(cx, |store, cx| store.command(command, cx)))
                    else {
                        return;
                    };
                    if let Err(error) = request.await {
                        let _ = cx.update(|window, cx| {
                            window.push_notification(
                                Notification::error(crate::tr!(
                                    "sidebar.undo_failed",
                                    reason = error.message
                                )),
                                cx,
                            )
                        });
                        return;
                    }
                }
                if let Some(id) = entry.reopen {
                    store.update(cx, |store, cx| store.put_back_thread(id, cx));
                }
            }
        })
        .detach();
    }

    pub(super) fn on_make_active(
        &mut self,
        action: &ThreadMakeActive,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.make_session_active(action.0.session_id.clone())
            });
        }
    }

    pub(super) fn on_archive(
        &mut self,
        action: &ThreadArchive,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = self.thread_title(&action.0, cx).unwrap_or_default();
        self.archive_thread(&action.0, &title, window, cx);
    }

    pub(super) fn on_delete(
        &mut self,
        action: &ThreadDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = self.thread_title(&action.0, cx).unwrap_or_default();
        self.delete_thread(&action.0, &title, window, cx);
    }

    /// Delete a draft at once: nothing is asked, and the deletion can be
    /// taken back while its notice shows.
    pub(super) fn discard_draft(
        &mut self,
        thread: &ThreadKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&thread.host) else {
            return;
        };
        let session_id = thread.session_id.clone();
        let reopen = store
            .read(cx)
            .is_thread_open(&session_id)
            .then(|| session_id.clone());
        store.update(cx, |store, _| {
            store.delete_session(session_id.clone(), false)
        });
        self.push_lifecycle_undo(
            UndoKind::DiscardDraft,
            UndoEntry {
                host: thread.host.clone(),
                commands: vec![Command::RestoreDraft { session_id }],
                reopen,
            },
            Vec::new(),
            window,
            cx,
        );
    }

    pub(super) fn on_project_thread_rules(
        &mut self,
        action: &ProjectThreadRules,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&action.0.host) else {
            return;
        };
        if let Some(project) = store.read(cx).project(&action.0.project_id).cloned() {
            crate::settings_page::thread_behavior::open_project_rules(store, project, window, cx);
        }
    }

    pub(super) fn on_project_archive_all(
        &mut self,
        action: &ProjectArchiveAll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&action.0.host) else {
            return;
        };
        let session_ids: Vec<String> = store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .filter(|meta| {
                meta.project_id.as_deref() == Some(action.0.project_id.as_str())
                    && meta.archived_at.is_none()
            })
            .map(|meta| meta.id.clone())
            .collect();
        if session_ids.is_empty() {
            return;
        }
        let count = session_ids.len();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let session_ids = session_ids.clone();
            alert
                .title(crate::tr!("sidebar.archive_all_title"))
                .description(crate::tr!("sidebar.archive_all_description", count = count))
                .button_props(
                    DialogButtons::default()
                        .ok_text(crate::tr!("sidebar.archive_all_action"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let requests: Vec<_> = session_ids
                        .iter()
                        .map(|session_id| {
                            store.update(cx, |store, cx| {
                                store.command(
                                    Command::ArchiveSession {
                                        session_id: session_id.clone(),
                                    },
                                    cx,
                                )
                            })
                        })
                        .collect();
                    // The host refuses a thread it will not archive (one still running);
                    // each distinct reason is shown once, with how many it kept.
                    window
                        .spawn(cx, async move |cx| {
                            let mut refused: Vec<(String, usize)> = Vec::new();
                            for request in requests {
                                if let Err(error) = request.await {
                                    match refused
                                        .iter_mut()
                                        .find(|(reason, _)| *reason == error.message)
                                    {
                                        Some((_, count)) => *count += 1,
                                        None => refused.push((error.message, 1)),
                                    }
                                }
                            }
                            let _ = cx.update(|window, cx| {
                                for (reason, count) in refused {
                                    window.push_notification(
                                        Notification::warning(crate::tr!(
                                            "sidebar.archive_all_refused",
                                            count = count,
                                            reason = reason
                                        )),
                                        cx,
                                    );
                                }
                            });
                        })
                        .detach();
                    true
                })
        });
    }

    pub(super) fn on_change_project_icon(
        &mut self,
        action: &ChangeProjectIcon,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&action.0.host) else {
            return;
        };
        let project = store.read(cx).project(&action.0.project_id).cloned();
        if let Some(project) = project {
            crate::project_icon::open(store, project, window, cx);
        }
    }

    pub(super) fn on_project_delete(
        &mut self,
        action: &ProjectDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            confirm_remove_project(store, action.0.project_id.clone(), window, cx);
        }
    }

    /// A project's share items, where its machine has spaces this client
    /// may manage.
    pub(super) fn share_target(
        &self,
        host: &HostKey,
        project_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<ShareTarget> {
        let project_id = project_id?;
        let store = self.store_of(host)?;
        let project_name = store.read(cx).project(project_id)?.name.clone();
        Some(ShareTarget {
            spaces: spaces::for_store(&store, cx)?,
            host: host.clone(),
            project_id: project_id.to_owned(),
            project_name,
        })
    }

    /// The badge of a project shared in a space; it fades in as the
    /// project is shared and out as it stops being.
    pub(super) fn shared_badge(
        &self,
        project: &ProjectKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let names = self
            .store_of(&project.host)
            .and_then(|store| spaces::for_store(&store, cx))
            .map(|spaces| spaces.read(cx).sharing(&project.project_id))
            .filter(|names| !names.is_empty());
        let name = self.name(&project.host, &project.project_id);
        let shown = crate::motion::shown(
            SharedString::from(format!("shared-badge-{name}")),
            names,
            SHOW,
            window,
            cx,
        )?;
        Some(shown.fade(spaces::shared_badge(&name, &shown.value, cx)))
    }

    pub(super) fn on_toggle_share(
        &mut self,
        action: &spaces::ToggleShare,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A space is one machine's: the host whose spaces hold it.
        let stores: Vec<_> = self.hosts.iter().map(|host| host.store.clone()).collect();
        let spaces = stores.iter().find_map(|store| {
            spaces::for_store(store, cx).filter(|spaces| {
                spaces
                    .read(cx)
                    .spaces()
                    .iter()
                    .any(|space| space.id == action.space_id)
            })
        });
        if let Some(spaces) = spaces {
            spaces::toggle_share(&spaces, action, window, cx);
        }
    }

    pub(super) fn on_new_space_and_share(
        &mut self,
        action: &spaces::NewSpaceAndShare,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(share) = self.share_target(&action.0, Some(&action.1), cx) {
            spaces::new_space_and_share(
                share.spaces,
                share.project_id,
                share.project_name,
                window,
                cx,
            );
        }
    }

    pub(super) fn on_change_project_root(
        &mut self,
        action: &ChangeProjectRoot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            crate::change_project_root_dialog::open(store, action.0.project_id.clone(), window, cx);
        }
    }

    pub(super) fn on_project_reveal(
        &mut self,
        action: &ProjectReveal,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(root) = self
            .store_of(&action.0.host)
            .and_then(|store| store.read(cx).project_root(&action.0.project_id))
        {
            cx.reveal_path(&root);
        }
    }

    /// Archive a thread, honoring the delete-confirmation setting.
    pub(super) fn archive_thread(
        &mut self,
        thread: &ThreadKey,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&thread.host) else {
            return;
        };
        let sidebar = cx.entity().downgrade();
        let commands = |session_id: &str| {
            (
                vec![Command::ArchiveSession {
                    session_id: session_id.to_owned(),
                }],
                Some((
                    UndoKind::Archive,
                    vec![Command::UnarchiveSession {
                        session_id: session_id.to_owned(),
                    }],
                )),
            )
        };
        if store.read(cx).settings().skip_delete_confirmation {
            let (archive, undo) = commands(&thread.session_id);
            self.perform_lifecycle(&thread.host, archive, undo, window, cx);
            return;
        }
        let title = title.to_string();
        let thread = thread.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let sidebar = sidebar.clone();
            let thread = thread.clone();
            alert
                .title(crate::tr!("sidebar.archive_title"))
                .description(crate::tr!("sidebar.archive_description", title = title))
                .button_props(
                    DialogButtons::default()
                        .ok_text(crate::tr!("sidebar.archive_action"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let _ = sidebar.update(cx, |this, cx| {
                        let (archive, undo) = commands(&thread.session_id);
                        this.perform_lifecycle(&thread.host, archive, undo, window, cx)
                    });
                    true
                })
        });
    }

    /// Permanently delete a thread and every thread under it: an optional
    /// confirm, then (when it orphans a worktree) a second "remove the worktree
    /// too?" prompt.
    pub(super) fn delete_thread(
        &mut self,
        thread: &ThreadKey,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&thread.host) else {
            return;
        };
        let session_id = thread.session_id.clone();
        let draft = store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .any(|meta| meta.id == session_id && meta.draft_text.is_some());
        if draft {
            self.discard_draft(thread, window, cx);
            return;
        }
        let skip = store.read(cx).settings().skip_delete_confirmation;
        if skip {
            proceed_delete(store, session_id, window, cx);
            return;
        }
        confirm_delete(
            store,
            session_id,
            title.to_string(),
            crate::tr!("sidebar.delete_action").into(),
            window,
            cx,
        );
    }
}
