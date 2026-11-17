use super::*;

/// The per-thread notification slot for its pull request watch.
struct PullRequestWatchNotice;

// ---------------------------------------------------------------------------
// Toasts and runtime events
// ---------------------------------------------------------------------------

impl AppShell {
    /// Present what `store`'s host tells the window.
    pub(super) fn present_app_event(
        &mut self,
        store: &Entity<WorkspaceStore>,
        event: &RuntimeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let toast = match event {
            // The language is this device's, whatever a host says.
            RuntimeEvent::Effect(RuntimeEffect::ApplyLocale { .. }) => return,
            RuntimeEvent::Effect(RuntimeEffect::CopyToClipboard { text }) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                return;
            }
            RuntimeEvent::Error(RuntimeError::WorkingDirectoryMissing {
                project_id: Some(project_id),
                cwd,
                ..
            }) => {
                crate::change_project_root_dialog::open_missing_directory(
                    store.clone(),
                    project_id.clone(),
                    cwd.clone(),
                    window,
                    cx,
                );
                return;
            }
            RuntimeEvent::Error(_) | RuntimeEvent::Notice(_) => {
                let presented = present_runtime_event(event);
                let message = self.said_by(store, presented.message, cx);
                let notification = match presented.severity {
                    RuntimeEventSeverity::Error => Notification::error(message),
                    RuntimeEventSeverity::Warning => Notification::warning(message),
                    RuntimeEventSeverity::Success => Notification::success(message),
                };
                window.push_notification(notification, cx);
                return;
            }
            RuntimeEvent::Toast(RuntimeToast::PullRequestWatch {
                session_id,
                number,
                notice,
            }) => {
                self.present_pull_request_watch(store, session_id, *number, notice, window, cx);
                return;
            }
            RuntimeEvent::Toast(RuntimeToast::PullRequestStack {
                session_id,
                target,
                stack,
                layers,
                base,
                result,
                late,
            }) => {
                crate::pull_requests::present_stack_result(
                    store, session_id, target, *stack, layers, base, result, *late, window, cx,
                );
                return;
            }
            RuntimeEvent::Toast(toast) => toast,
        };

        let presented = present_runtime_toast(toast);
        let host = store.read(cx).host_key();
        let toast_id = match presented.disposition {
            RuntimeToastDisposition::Push => self.take_toast_id(),
            // A window that opened after an operation started hears of it
            // first from its progress.
            RuntimeToastDisposition::Start(operation)
            | RuntimeToastDisposition::Update(operation) => {
                match self.operation_toasts.get(&(host.clone(), operation)) {
                    Some(id) => *id,
                    None => {
                        let toast_id = self.take_toast_id();
                        self.operation_toasts.insert((host, operation), toast_id);
                        toast_id
                    }
                }
            }
            RuntimeToastDisposition::Finish(operation) => self
                .operation_toasts
                .remove(&(host, operation))
                .unwrap_or_else(|| self.take_toast_id()),
        };

        let action = presented.action.map(|action| {
            let store = store.clone();
            let label = match &action {
                RuntimeToastAction::RetryGit(_) => crate::tr!("git.toast.retry"),
                RuntimeToastAction::UpdateProviders(_) => crate::tr!("providers.update_all"),
                RuntimeToastAction::CancelPluginOperation(_) => {
                    crate::tr!("providers.plugins.toast.cancel")
                }
                RuntimeToastAction::CancelAcpInstall(_) => {
                    crate::tr!("providers.acp.cancel_install")
                }
                RuntimeToastAction::CancelProviderUpdate => crate::tr!("providers.cancel_update"),
            }
            .into_owned()
            .into();
            ToastAction {
                label,
                handler: Rc::new(move |window, cx| {
                    window.remove_notification1::<RuntimeToastNotification>(toast_id as usize, cx);
                    store.update(cx, |store, _cx| match action.clone() {
                        RuntimeToastAction::RetryGit(request) => store.retry_git_action(request),
                        RuntimeToastAction::UpdateProviders(providers) => {
                            store.update_providers(providers)
                        }
                        RuntimeToastAction::CancelPluginOperation(profile_id) => {
                            store.cancel_plugin_operation(profile_id)
                        }
                        RuntimeToastAction::CancelAcpInstall(id) => store.cancel_acp_install(id),
                        RuntimeToastAction::CancelProviderUpdate => store.cancel_provider_update(),
                    });
                }),
            }
        });
        let title = self.said_by(store, presented.title, cx);
        self.show_toast(
            toast_id,
            presented.kind,
            (title, presented.detail, presented.progress),
            action,
            window,
            cx,
        );
    }

    /// `text` from `store`'s host, after the host's name once the window is
    /// over several.
    pub(super) fn said_by(&self, store: &Entity<WorkspaceStore>, text: String, cx: &App) -> String {
        if self.links.len() > 1 {
            crate::tr!(
                "toast.on_host",
                host = store.read(cx).host_title(),
                message = text
            )
            .into_owned()
        } else {
            text
        }
    }

    /// One slot per thread: a newer wake or end of its watch replaces the older notice.
    pub(super) fn present_pull_request_watch(
        &mut self,
        store: &Entity<WorkspaceStore>,
        session_id: &str,
        number: u64,
        notice: &tcode_core::pull_request_watch::WatchNotice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A pane's transcript already shows the message.
        if window.is_window_active()
            && self
                .pane_showing(&store.read(cx).thread_key(session_id), cx)
                .is_some()
        {
            return;
        }
        let store = store.clone();
        let title = store
            .read(cx)
            .thread_meta(session_id)
            .map(|meta| meta.title.clone())
            .unwrap_or_default();
        let title = self.said_by(&store, title, cx);
        let (kind, message) = crate::pull_requests::watch_notice_toast(number, notice);
        let id = session_id.to_owned();
        // A session id is unique only on its host.
        let slot = SharedString::from(format!("{}/{id}", store.read(cx).host_key().name()));
        window.push_notification(
            Notification::new()
                .id1::<PullRequestWatchNotice>(slot.clone())
                .title(title)
                .message(message)
                .with_type(kind)
                .action(move |_, _, _| {
                    use crate::sizing::Sizable as _;
                    let store = store.clone();
                    let id = id.clone();
                    let slot = slot.clone();
                    crate::widgets::button::Button::new("pull-request-watch-open-thread")
                        .small()
                        .outline()
                        .label(crate::tr!("pull_requests.open_thread"))
                        .on_click(move |_, window, cx| {
                            window.remove_notification1::<PullRequestWatchNotice>(slot.clone(), cx);
                            store.update(cx, |store, cx| store.show_thread(id.clone(), cx));
                        })
                })
                .autohide(true),
            cx,
        );
    }

    pub(super) fn take_toast_id(&mut self) -> ToastId {
        let id = self.next_toast_id;
        self.next_toast_id += 1;
        id
    }

    pub(super) fn show_toast(
        &mut self,
        id: ToastId,
        kind: ToastKind,
        content: (String, Option<String>, Option<f32>),
        action: Option<ToastAction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, detail, progress) = content;
        let autohide = action.is_none()
            && matches!(
                kind,
                ToastKind::Success | ToastKind::Info | ToastKind::Warning
            );
        window.push_notification(
            crate::toast::notification(id, kind, title, detail.map(Into::into), action, progress)
                .autohide(autohide),
            cx,
        );
    }

    pub(super) fn on_open_source_control(
        &mut self,
        _: &crate::pull_requests::OpenSourceControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
        self.window_state.update(cx, |state, cx| {
            state.pending_settings_section = Some("source_control".into());
            state.open_settings(cx);
        });
    }

    pub(super) fn on_open_thread_anchor(
        &mut self,
        anchor: &crate::markdown::OpenThreadAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store(cx) else {
            return;
        };
        if store.read(cx).thread_meta(&anchor.thread_id).is_none() {
            window.push_notification(
                crate::overlay::Notification::warning(
                    crate::tr!("control.anchor_missing").into_owned(),
                ),
                cx,
            );
            return;
        }
        let anchor = anchor.clone();
        store.update(cx, |store, cx| match anchor.position {
            Some(position) => store.show_thread_at_position(anchor.thread_id, position, cx),
            None => store.show_thread(anchor.thread_id, cx),
        });
    }

    pub(super) fn on_thread_undo(
        &mut self,
        _: &crate::sidebar::ThreadUndo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.undo_lifecycle(window, cx));
    }

    pub(super) fn on_toggle_palette(
        &mut self,
        _: &TogglePalette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window_state
            .update(cx, |state, cx| state.toggle_palette(cx));
    }
}
