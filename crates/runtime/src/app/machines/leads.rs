//! What a thread here owes a thread on another machine of the group: the report and the
//! requests of a subagent that thread started here, and the firings of the watches it put
//! on threads here.
use tcode_core::control::{RemoteChildState, RemoteParent, RemoteWatch};
use tcode_core::settlement::AgentReport;
use tcode_protocol::MachineRequest;

use crate::app::*;

/// What names the sending of report `report` of `child_id`.
pub(super) fn report_key(child_id: &str, report: &str) -> String {
    format!("report:{child_id}:{report}")
}

/// The other machine no longer takes it: its thread is gone, archived, or no longer waits
/// on this one.
fn not_taken(error: &ProtocolError) -> bool {
    error.code == "out_of_scope"
}

impl AppState {
    /// Send the report `child_id` keeps for its lead on another machine, which keeps it
    /// until that lead's harness has taken it.
    pub(in crate::app) fn send_report_to_lead(&mut self, child_id: &str, cx: &mut HostCx) {
        let Some(meta) = self.find_meta(child_id) else {
            return;
        };
        let Some(parent) = meta.remote_parent.clone() else {
            return;
        };
        let Some(expectation) = meta.expectation.as_ref() else {
            return;
        };
        let (Some(text), Some(report)) = (
            expectation.undelivered.clone(),
            expectation.undelivered_id.clone(),
        ) else {
            return;
        };
        let state = match expectation.state {
            AgentReport::Failed => RemoteChildState::Failed,
            _ => RemoteChildState::Reported,
        };
        let key = report_key(child_id, &report);
        let request = MachineRequest::Callback {
            thread_id: parent.thread_id,
            from_thread_id: child_id.to_owned(),
            from_title: meta.title,
            text,
            report: Some(report.clone()),
            state: Some(state),
            watch: None,
        };
        let child_id = child_id.to_owned();
        self.machine_work
            .reports_sent
            .insert(child_id.clone(), report.clone());
        self.call_machine(
            &parent.machine,
            Some(key.clone()),
            request,
            false,
            cx,
            move |state, answer, cx| match answer {
                Ok(_) => {
                    state.machine_work.queued_there.insert(key);
                }
                Err(error) if not_taken(&error) => {
                    state.change_meta(&child_id, cx, |meta| {
                        if let Some(expectation) = meta.expectation.as_mut().filter(|expectation| {
                            expectation.undelivered_id.as_deref() == Some(report.as_str())
                        }) {
                            expectation.undelivered = None;
                            expectation.undelivered_id = None;
                        }
                    });
                }
                // Kept, and sent again on the next contact with that machine.
                Err(_) => {}
            },
        );
    }

    /// The reports this machine's threads keep for leads on `machine`, or on every machine,
    /// that the lead's machine has not queued yet.
    pub(super) fn send_reports_to_leads(&mut self, machine: Option<&str>, cx: &mut HostCx) {
        let due: Vec<String> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.remote_parent
                    .as_ref()
                    .is_some_and(|parent| machine.is_none_or(|machine| parent.machine == machine))
            })
            .filter(|meta| {
                meta.expectation.as_ref().is_some_and(|expectation| {
                    expectation.undelivered.is_some()
                        && expectation.undelivered_id.as_ref().is_some_and(|report| {
                            !self
                                .machine_work
                                .queued_there
                                .contains(&report_key(&meta.id, report))
                        })
                })
            })
            .map(|meta| meta.id.clone())
            .collect();
        for child in due {
            self.send_report_to_lead(&child, cx);
        }
    }

    /// A subagent's approval request or question for its lead on another machine, sent
    /// once: it lives no longer than the subagent's provider.
    pub(in crate::app) fn send_callback_to_lead(
        &mut self,
        child: &SessionMeta,
        parent: &RemoteParent,
        text: String,
        cx: &mut HostCx,
    ) {
        let request = MachineRequest::Callback {
            thread_id: parent.thread_id.clone(),
            from_thread_id: child.id.clone(),
            from_title: child.title.clone(),
            text,
            report: None,
            state: None,
            watch: None,
        };
        self.call_machine(&parent.machine, None, request, false, cx, |_, _, _| {});
    }

    /// Threads `ids` here are archived or deleted: the leads elsewhere of those that still
    /// owed a report hear they will get none, once, and no longer wait on them.
    pub(in crate::app) fn remote_children_gone<'a>(
        &mut self,
        ids: impl IntoIterator<Item = &'a str>,
        cx: &mut HostCx,
    ) {
        let gone: Vec<(SessionMeta, RemoteParent)> = ids
            .into_iter()
            .filter_map(|id| self.find_meta(id))
            .filter(|meta| {
                meta.cancelled_at.is_none()
                    && meta
                        .expectation
                        .as_ref()
                        .is_some_and(|expectation| expectation.state.pending())
            })
            .filter_map(|meta| {
                let parent = meta.remote_parent.clone()?;
                Some((meta, parent))
            })
            .collect();
        for (child, parent) in gone {
            let request = MachineRequest::Callback {
                thread_id: parent.thread_id.clone(),
                from_thread_id: child.id.clone(),
                from_title: child.title.clone(),
                text: String::new(),
                report: None,
                state: Some(RemoteChildState::Cancelled),
                watch: None,
            };
            self.call_machine(&parent.machine, None, request, false, cx, |_, _, _| {});
        }
    }

    /// Send the watch firings `thread` keeps for threads on other machines.
    pub(in crate::app) fn send_watch_firings(&mut self, thread: Option<&str>, cx: &mut HostCx) {
        self.send_watch_firings_where(thread, None, cx);
    }

    /// Send the watch firings kept for threads on `machine`, or on every machine.
    pub(super) fn send_watch_firings_to(&mut self, machine: Option<&str>, cx: &mut HostCx) {
        self.send_watch_firings_where(None, machine, cx);
    }

    fn send_watch_firings_where(
        &mut self,
        thread: Option<&str>,
        machine: Option<&str>,
        cx: &mut HostCx,
    ) {
        let due: Vec<(String, String, RemoteWatch, String)> = self
            .sessions
            .iter()
            .filter(|meta| thread.is_none_or(|thread| meta.id == thread))
            .flat_map(|meta| {
                meta.remote_watches
                    .iter()
                    .filter(|watch| {
                        watch.fired.is_some()
                            && machine.is_none_or(|machine| watch.machine == machine)
                    })
                    .map(|watch| {
                        (
                            meta.id.clone(),
                            meta.title.clone(),
                            watch.clone(),
                            watch.fired.clone().unwrap_or_default(),
                        )
                    })
            })
            .collect();
        for (target, title, watch, text) in due {
            let key = format!(
                "watch:{target}:{}:{}:{}",
                watch.machine, watch.thread_id, watch.id
            );
            let request = MachineRequest::Callback {
                thread_id: watch.thread_id.clone(),
                from_thread_id: target.clone(),
                from_title: title,
                text,
                report: None,
                state: None,
                watch: Some(watch.id.clone()),
            };
            self.call_machine(
                &watch.machine.clone(),
                Some(key),
                request,
                false,
                cx,
                move |state, answer, cx| {
                    let delivered = match &answer {
                        Ok(_) => true,
                        Err(error) => not_taken(error),
                    };
                    if delivered {
                        state.change_meta(&target, cx, |meta| {
                            meta.remote_watches.retain(|kept| {
                                !(kept.id == watch.id
                                    && kept.machine == watch.machine
                                    && kept.thread_id == watch.thread_id
                                    && kept.fired.is_some())
                            });
                        });
                    }
                },
            );
        }
    }
}
