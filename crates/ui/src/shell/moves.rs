//! What the window does about a project that moved to another machine of its
//! group: it says so when the move ends, and a thread it showed, a link or
//! the remembered thread that names one of the project's threads is followed
//! to the machine the thread is on now.

use super::*;
use crate::panes::MovedAway;
use crate::store::{HostKey, ThreadKey};
use crate::toast::{ToastAction, ToastKind};
use tcode_protocol::{EndedProjectMove, MoveOutcome, ProtocolError};

/// A thread to show once the machine it moved to lists it.
pub(super) struct PendingMoved {
    thread: ThreadKey,
    position: Option<u64>,
    pane: Option<WeakEntity<ThreadPanel>>,
    /// The thread is known to have gone there: it is waited for until the
    /// machine lists it, rather than given up on as missing.
    moved: bool,
}

/// A thread of a machine of the group, by the machine's id.
pub(crate) struct MachineThread {
    pub machine: String,
    pub thread_id: String,
    pub position: Option<u64>,
    /// Its host said it went there.
    pub moved: bool,
}

impl AppShell {
    /// The host this window shows as machine `machine` of a group.
    fn machine_host(&self, machine: &str, cx: &mut Context<Self>) -> Option<HostKey> {
        let stores: Vec<_> = self
            .links
            .iter()
            .map(|link| (link.key(), link.store.clone()))
            .collect();
        stores.into_iter().find_map(|(host, store)| {
            let found = match &host {
                HostKey::Remote(id) => id == machine,
                HostKey::Local => crate::machines::group_machines(&store, cx)
                    .iter()
                    .any(|known| known.this_machine && known.id == machine),
            };
            found.then_some(host)
        })
    }

    pub(super) fn present_project_move_ended(
        &mut self,
        store: &Entity<WorkspaceStore>,
        ended: &EndedProjectMove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The open progress of the move says how it ended.
        if store.read(cx).project_move_watched(&ended.project_id) {
            return;
        }
        let machine =
            crate::machines::machine_in_groups(std::slice::from_ref(store), &ended.machine, cx)
                .name;
        let id = self.take_toast_id();
        if matches!(ended.outcome, MoveOutcome::Cancelled { .. }) {
            self.show_toast(
                id,
                ToastKind::Info,
                (
                    crate::tr!(
                        "machines.move.cancelled",
                        project = ended.project_name.clone()
                    )
                    .into_owned(),
                    None,
                    None,
                ),
                None,
                window,
                cx,
            );
            return;
        }
        let action = self.machine_host(&ended.machine, cx).map(|host| {
            let shell = cx.entity().downgrade();
            let threads = ended.thread_ids.clone();
            ToastAction {
                label: crate::tr!("machines.move.open").into_owned().into(),
                handler: Rc::new(move |_, cx| {
                    let _ = shell.update(cx, |shell, cx| {
                        shell.open_moved_project(&host, &threads, cx)
                    });
                }),
            }
        });
        self.show_toast(
            id,
            ToastKind::Success,
            (
                crate::tr!(
                    "machines.move.moved",
                    project = ended.project_name.clone(),
                    machine = machine
                )
                .into_owned(),
                None,
                None,
            ),
            action,
            window,
            cx,
        );
    }

    /// `host`'s section, at the latest of the threads that moved there.
    fn open_moved_project(&mut self, host: &HostKey, threads: &[String], cx: &mut Context<Self>) {
        self.show_host_section(host, cx);
        let Some(store) = self.link_of(host).map(|link| link.store.clone()) else {
            return;
        };
        let latest = store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .find(|meta| threads.contains(&meta.id))
            .map(|meta| meta.id.clone());
        if let Some(latest) = latest {
            store.update(cx, |store, cx| store.show_thread(latest, cx));
        }
    }

    /// A pane whose thread `store` closed because it went with its project:
    /// the pane follows it, or says where it went.
    pub(super) fn pane_thread_moved(
        &mut self,
        pane: &Entity<ThreadPanel>,
        session_id: &str,
        machine: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.follow_machine_thread(
            MachineThread {
                machine,
                thread_id: session_id.to_owned(),
                position: None,
                moved: true,
            },
            Some(pane.clone()),
            window,
            cx,
        );
    }

    /// A pane whose draft `store` closed because its project went to
    /// `machine`: no thread went with it, so the pane says where the project
    /// is, with the way to that machine.
    pub(super) fn pane_draft_moved(
        &mut self,
        pane: &Entity<ThreadPanel>,
        machine: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, open) = self.machine_opener(&machine, cx);
        pane.update(cx, |pane, cx| {
            pane.show_moved_away(
                MovedAway {
                    machine: name,
                    open,
                    project: true,
                },
                cx,
            )
        });
    }

    /// What the group calls `machine`, and opening it in this window: its
    /// section where the window shows it, else the machine this client saved.
    fn machine_opener(
        &mut self,
        machine: &str,
        cx: &mut Context<Self>,
    ) -> (String, Option<(String, crate::panes::OpenMachine)>) {
        let stores: Vec<_> = self.links.iter().map(|link| link.store.clone()).collect();
        let name = crate::machines::machine_in_groups(&stores, machine, cx).name;
        let label = crate::tr!("machines.move.open_machine", machine = name.clone()).into_owned();
        let shell = cx.entity().downgrade();
        if let Some(host) = self.machine_host(machine, cx) {
            let open: crate::panes::OpenMachine = Rc::new(move |_, cx| {
                let _ = shell.update(cx, |shell, cx| shell.show_host_section(&host, cx));
            });
            return (name, Some((label, open)));
        }
        let saved = self
            .client
            .load_hosts()
            .unwrap_or_default()
            .into_iter()
            .find(|saved| saved.host_id == machine && saved.space_id.is_none());
        let open = saved.map(|saved| {
            let open: crate::panes::OpenMachine = Rc::new(move |window, cx| {
                let saved = saved.clone();
                let _ = shell.update(cx, |shell, cx| shell.open_paired(saved, window, cx));
            });
            (label, open)
        });
        (name, open)
    }

    /// `store` no longer has `thread_id`: ask it where the thread went, and
    /// follow it there; `unplaced` runs when it went nowhere (`None`) or the
    /// host could not say, with its error: the caller knows whether the user
    /// asked for the thread.
    pub(super) fn find_moved_thread(
        &mut self,
        store: &Entity<WorkspaceStore>,
        thread_id: String,
        position: Option<u64>,
        unplaced: impl FnOnce(Option<ProtocolError>, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = store.update(cx, |store, cx| store.moved_thread(thread_id.clone(), cx));
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            let _ = this.update_in(cx, |shell, window, cx| match answer {
                Ok(Some(machine)) => shell.follow_machine_thread(
                    MachineThread {
                        machine,
                        thread_id,
                        position,
                        moved: true,
                    },
                    None,
                    window,
                    cx,
                ),
                Ok(None) => unplaced(None, window, cx),
                Err(error) => unplaced(Some(error), window, cx),
            });
        })
        .detach();
    }

    /// Show `target` on its machine where this window has it open; else,
    /// in `pane` or the current one, where it is and how to open that
    /// machine.
    pub(super) fn follow_machine_thread(
        &mut self,
        target: MachineThread,
        pane: Option<Entity<ThreadPanel>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(host) = self.machine_host(&target.machine, cx) {
            let pending = PendingMoved {
                thread: ThreadKey {
                    host,
                    session_id: target.thread_id,
                },
                position: target.position,
                pane: pane.map(|pane| pane.downgrade()),
                moved: target.moved,
            };
            let waiting = self
                .pending_moved
                .iter()
                .any(|waiting| waiting.thread == pending.thread);
            if !waiting && !self.show_pending_moved(&pending, window, cx) {
                self.pending_moved.push(pending);
            }
            return;
        }
        let (machine, open) = self.machine_opener(&target.machine, cx);
        // A link followed from a thread being read leaves that thread where
        // it is: the window says where the other one went instead.
        let pane = pane.or_else(|| {
            let current = self.column.as_ref()?.current_pane.clone();
            let live = current.read(cx).session_id().is_some_and(|session_id| {
                current.read(cx).store().read(cx).is_thread_open(session_id)
            });
            (!live).then_some(current)
        });
        match pane {
            Some(pane) => pane.update(cx, |pane, cx| {
                pane.show_moved_away(
                    MovedAway {
                        machine,
                        open,
                        project: false,
                    },
                    cx,
                )
            }),
            None => {
                let text = crate::tr!("machines.move.thread_moved", machine = machine).into_owned();
                let mut note = Notification::info(text);
                if let Some((label, open)) = open {
                    note = note.action(move |_, _, _| {
                        let open = open.clone();
                        crate::widgets::button::Button::new("moved-away-notice-open")
                            .label(label.clone())
                            .on_click(move |_, window, cx| open(window, cx))
                    });
                }
                window.push_notification(note, cx);
            }
        }
    }

    /// Show a thread that moved here once its machine lists it: whether it
    /// is shown now.
    fn show_pending_moved(
        &mut self,
        pending: &PendingMoved,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(store) = self
            .link_of(&pending.thread.host)
            .map(|link| link.store.clone())
        else {
            return true;
        };
        if store
            .read(cx)
            .thread_meta(&pending.thread.session_id)
            .is_none()
        {
            // A machine that has listed its threads and not this one does
            // not have it.
            if !pending.moved && store.read(cx).index_hydrated() {
                window.push_notification(
                    Notification::warning(crate::tr!("control.anchor_missing").into_owned()),
                    cx,
                );
                return true;
            }
            return false;
        }
        let pane = pending.pane.as_ref().and_then(WeakEntity::upgrade);
        match (pane, pending.position) {
            (Some(pane), None) => self.show_in_pane(&pane, &pending.thread, false, window, cx),
            (_, position) => {
                let id = pending.thread.session_id.clone();
                store.update(cx, |store, cx| match position {
                    Some(position) => store.show_thread_at_position(id, position, cx),
                    None => store.show_thread(id, cx),
                });
            }
        }
        true
    }

    /// The threads waiting for `host` to list them that it lists now.
    pub(super) fn reconcile_pending_moved(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .pending_moved
            .iter()
            .any(|pending| pending.thread.host == *host)
        {
            return;
        }
        let pending = std::mem::take(&mut self.pending_moved);
        for waiting in pending {
            if waiting.thread.host != *host || !self.show_pending_moved(&waiting, window, cx) {
                self.pending_moved.push(waiting);
            }
        }
    }

    pub(super) fn on_open_machine_thread_anchor(
        &mut self,
        anchor: &crate::markdown::OpenMachineThreadAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.follow_machine_thread(
            MachineThread {
                machine: anchor.machine.clone(),
                thread_id: anchor.thread_id.clone(),
                position: anchor.position,
                moved: false,
            },
            None,
            window,
            cx,
        );
    }
}
