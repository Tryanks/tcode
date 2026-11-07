//! Telling the user through the operating system that a thread waits on
//! them while Tcode is not in front: an approval, or computer use's question
//! whether an app may come to the front to type, which the host reports the
//! same way. The host holds the question; the client only says so, and takes
//! the notice back once nothing waits.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{App, Entity, Task};
use tcode_client::host::SystemNotifier;

use crate::store::WorkspaceStore;

pub(crate) struct AttentionNotices {
    notifier: Rc<dyn SystemNotifier>,
    /// The threads known to wait, so each is told once.
    waiting: HashSet<String>,
    _opened: Task<()>,
}

impl AttentionNotices {
    /// Follow `store` on this client's notifier, if it has one. A click opens
    /// the thread here, in front.
    pub(crate) fn new(store: &Entity<WorkspaceStore>, cx: &mut App) -> Option<Self> {
        let notifier = cx
            .try_global::<crate::remote::ClientAttachment>()?
            .host()
            .system_notifier()?;
        let opened = notifier.opened();
        let store = store.downgrade();
        let opened = cx.spawn(async move |cx| {
            while let Ok(thread_id) = opened.recv().await {
                let shown = store.update(cx, |store, cx| {
                    store.show_thread(thread_id, cx);
                    cx.activate(true);
                });
                if shown.is_err() {
                    return;
                }
            }
        });
        Some(Self {
            notifier,
            waiting: HashSet::new(),
            _opened: opened,
        })
    }

    pub(crate) fn sync(&mut self, store: &WorkspaceStore, cx: &App) {
        let frontmost = cx.active_window().is_some();
        let waiting: HashSet<String> = store
            .sidebar_sessions()
            .iter()
            .filter(|meta| store.pending_approval_for(&meta.id))
            .map(|meta| meta.id.clone())
            .collect();
        for thread_id in self.waiting.difference(&waiting) {
            self.notifier.withdraw(thread_id);
        }
        if !frontmost {
            for thread_id in waiting.difference(&self.waiting) {
                if let Some(meta) = store.thread_meta(thread_id) {
                    let body = crate::tr!(
                        "system_notifications.waiting",
                        harness = store.harness_name_of(meta)
                    );
                    self.notifier.post(thread_id, &meta.title, &body);
                }
            }
        }
        self.waiting = waiting;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use tcode_client::host::{ClientHost, Transport};
    use tcode_client::pairing::PairedHost;
    use tcode_core::project::SessionMeta;
    use tcode_protocol::{
        EventEnvelope, ForkAvailability, HostMessage, IndexSnapshot, IndexSummary, ServerEvent,
        SessionActivity, Topic, encode_line,
    };

    struct Recorded {
        posted: RefCell<Vec<(String, String, String)>>,
        withdrawn: RefCell<Vec<String>>,
        clicks: (
            async_channel::Sender<String>,
            async_channel::Receiver<String>,
        ),
    }

    impl SystemNotifier for Recorded {
        fn post(&self, thread_id: &str, title: &str, body: &str) {
            self.posted
                .borrow_mut()
                .push((thread_id.into(), title.into(), body.into()));
        }
        fn withdraw(&self, thread_id: &str) {
            self.withdrawn.borrow_mut().push(thread_id.into());
        }
        fn opened(&self) -> async_channel::Receiver<String> {
            self.clicks.1.clone()
        }
    }

    struct Desk(Rc<Recorded>);

    impl ClientHost for Desk {
        fn system_notifier(&self) -> Option<Rc<dyn SystemNotifier>> {
            Some(self.0.clone())
        }
        fn device_name(&self) -> String {
            "Desk".into()
        }
        fn device_id(&self) -> String {
            "desk".into()
        }
        fn device_platform(&self) -> Option<String> {
            None
        }
        fn load_hosts(&self) -> Vec<PairedHost> {
            Vec::new()
        }
        fn load_preferences(&self) -> tcode_client::host::ClientPreferences {
            Default::default()
        }
        fn save_preferences(&self, _: &tcode_client::host::ClientPreferences) {}
        fn save_hosts(&self, _: &[PairedHost]) {}
        fn last_host_id(&self) -> Option<String> {
            None
        }
        fn set_last_host_id(&self, _: Option<&str>) {}
        fn connect(&self, _: &PairedHost) -> Transport {
            unreachable!("the test attaches a store directly")
        }
    }

    /// The host's index with one thread, waiting on the user or not.
    fn index(meta: &SessionMeta, waiting: bool) -> String {
        encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: IndexSummary {
                    activity: HashMap::from([(
                        meta.id.clone(),
                        SessionActivity {
                            working: false,
                            turn_running: false,
                            waiting: false,
                            waiting_for_approval: waiting,
                            waiting_for_input: false,
                            failed: false,
                            unread: false,
                            fork: ForkAvailability::Available,
                            agent: None,
                            latest_activity: None,
                        },
                    )]),
                    ..Default::default()
                },
                sessions: vec![meta.clone()],
                projects: Vec::new(),
            }),
        }))
        .unwrap()
    }

    /// Away from Tcode a thread that starts to wait is told once and taken
    /// back when it stops; in front the window already shows it. A click
    /// opens the thread.
    #[gpui::test]
    fn a_thread_waiting_while_tcode_is_away_is_told_once_and_opens_on_click(
        cx: &mut TestAppContext,
    ) {
        let recorded = Rc::new(Recorded {
            posted: RefCell::default(),
            withdrawn: RefCell::default(),
            clicks: async_channel::unbounded(),
        });
        let desk = Rc::new(Desk(recorded.clone()));
        cx.update(|cx| {
            crate::theme::init(cx);
            cx.set_global(crate::remote::ClientAttachment::new(
                desk,
                true,
                |_, _, _| {},
            ));
        });
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump = link.clone();
        let executor = cx.background_executor.clone();
        cx.background_executor
            .spawn(async move { pump.pump_with_timer(|delay| executor.timer(delay)).await })
            .detach();
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        let mut meta = SessionMeta::new(agent::ProviderKind::Codex, "/work".into(), None);
        meta.title = "Release".into();
        let (mut notices, window) = {
            let notices = cx.update(|cx| AttentionNotices::new(&store, cx)).unwrap();
            (notices, cx.add_empty_window())
        };
        let window: &mut gpui::VisualTestContext = window;
        let mut show = |waiting: bool, cx: &mut gpui::VisualTestContext| {
            incoming.try_send(index(&meta, waiting)).unwrap();
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.update(|_, cx| notices.sync(store.read(cx), cx));
        };

        window.deactivate_window();
        window.run_until_parked();
        show(true, window);
        show(true, window);
        assert_eq!(
            *recorded.posted.borrow(),
            [(
                meta.id.clone(),
                "Release".to_string(),
                "Codex is waiting for your decision.".to_string()
            )]
        );
        show(false, window);
        assert_eq!(*recorded.withdrawn.borrow(), [meta.id.clone()]);

        window.update(|window, _| window.activate_window());
        window.run_until_parked();
        show(true, window);
        assert_eq!(recorded.posted.borrow().len(), 1, "the window shows it");

        let shown = Rc::new(RefCell::new(None));
        let _shown = window.update(|_, cx| {
            let shown = shown.clone();
            cx.subscribe(&store, move |_, event: &crate::store::ShowThread, _| {
                *shown.borrow_mut() = Some(event.session_id.clone());
            })
        });
        recorded.clicks.0.try_send(meta.id.clone()).unwrap();
        window.run_until_parked();
        assert_eq!(*shown.borrow(), Some(meta.id.clone()));
    }
}
