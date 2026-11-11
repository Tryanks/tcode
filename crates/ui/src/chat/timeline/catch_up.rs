//! A thread that ran while it was not shown, come back to: what the reader
//! sees once its window arrives, against a real host.

use std::sync::{Arc, Mutex};

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext, px, size};
use tcode_protocol::{ClientMessage, ClientPayload, Subscription, Topic};

use crate::chat::ChatView;
use crate::chat::tests::{answer_event, record_thread, thread_meta, user_event};
use crate::store::WorkspaceStore;
use crate::store::tests::LocalHostDriver;
use crate::test_frames::draw_settled;
use crate::window_state::WindowState;

fn turn(turn: usize, answer: &str) -> Vec<agent::AgentEvent> {
    vec![
        agent::AgentEvent::TurnStarted {
            turn_id: turn.to_string(),
        },
        user_event(&format!("user-{turn}"), &format!("Question {turn}")),
        answer_event(&format!("answer-{turn}"), answer),
        agent::AgentEvent::TurnCompleted {
            turn_id: turn.to_string(),
            status: agent::TurnStatus::Completed,
            usage: None,
        },
    ]
}

/// A provider's turn of `count` commands, each holding the four kilobytes of
/// output a record keeps, so 140 of them (560 KB) are more than a 512 KB
/// window.
fn long_run(turn: usize, count: usize) -> Vec<agent::AgentEvent> {
    let mut events = vec![agent::AgentEvent::TurnStarted {
        turn_id: turn.to_string(),
    }];
    for index in 0..count {
        events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: format!("command-{index}"),
            parent_item_id: None,
            content: agent::ItemContent::CommandExecution {
                command: format!("check {index}"),
                output: format!("{index} checked\n").repeat(350),
                exit_code: Some(0),
                status: agent::ItemStatus::Completed,
            },
        }));
    }
    events.push(answer_event("finished", "Every step is done."));
    events.push(agent::AgentEvent::TurnCompleted {
        turn_id: turn.to_string(),
        status: agent::TurnStatus::Completed,
        usage: None,
    });
    events
}

struct Workspace {
    host: tcode_runtime::pipe::SpawnedHost,
    driver: LocalHostDriver,
    store: Entity<WorkspaceStore>,
    /// Every message the client sent its host.
    sent: Arc<Mutex<Vec<ClientMessage>>>,
    first: String,
    second: String,
}

impl Workspace {
    /// A host with two threads, the first holding three finished turns, and
    /// a client attached to it whose messages are recorded on their way.
    fn new(cx: &mut TestAppContext) -> Self {
        use tcode_runtime::pipe::{HostServices, spawn_host};
        cx.update(crate::test_client::init);
        cx.update(crate::markdown::init);
        let root = crate::store::tests::scratch_root("catch-up");
        let disk = tcode_services::store::SessionStore::open_at(root).unwrap();
        let (first, second) = (thread_meta(None), thread_meta(None));
        record_thread(
            &disk,
            &first,
            (0..3).flat_map(|index| turn(index, "An answer.\n\nWith a second paragraph.")),
        );
        record_thread(&disk, &second, []);
        let (first, second) = (first.id, second.id);
        let host = spawn_host(disk, HostServices::default()).unwrap();
        let (to_host, requests) = async_channel::unbounded::<String>();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let recorded = sent.clone();
        let host_sender = host.to_host.clone();
        std::thread::spawn(move || {
            while let Ok(line) = requests.recv_blocking() {
                recorded
                    .lock()
                    .unwrap()
                    .push(tcode_protocol::decode_client_line(&line).unwrap());
                if host_sender.send_blocking(line).is_err() {
                    break;
                }
            }
            // The client closed its link: the host's inbox closes with it.
            host_sender.close();
        });
        let mut driver = LocalHostDriver::from_link(tcode_client::HostLink::new(
            to_host,
            host.from_host.clone(),
        ));
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        driver.wait_until(cx, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        Self {
            host,
            driver,
            store,
            sent,
            first,
            second,
        }
    }

    /// The user sends `thread` a turn, and its provider, a scripted one,
    /// reports `events`, as it does whether or not any client shows the
    /// thread.
    fn provider(
        &mut self,
        thread: &str,
        events: Vec<agent::AgentEvent>,
        cx: &mut TestAppContext,
    ) -> crate::chat::tests::ScriptedHandles {
        let scripted = crate::chat::tests::scripted_turn(
            &self.host,
            &mut self.driver,
            thread,
            agent::ProviderKind::ClaudeCode,
            cx,
        );
        for event in events {
            scripted.1.try_send(event).unwrap();
        }
        // The run is over on the host before anyone comes back to it: its
        // log ends with the turn's end.
        loop {
            let link = self.driver.link.clone();
            let page = self.driver.await_with_events(
                cx,
                link.query(tcode_protocol::Query::SessionHistoryPage {
                    session_id: thread.to_owned(),
                    before: u64::MAX,
                    limit: 1,
                }),
            );
            if let Ok(tcode_protocol::QueryResponse::SessionHistoryPage { records, .. }) = page
                && records.last().is_some_and(|record| {
                    matches!(record.event, agent::AgentEvent::TurnCompleted { .. })
                })
            {
                return scripted;
            }
            self.driver.wait_for_host("the run to finish");
        }
    }

    /// Show `thread` in `chat` as the shell does when its pane switches.
    fn show(&self, chat: &Entity<ChatView>, thread: &str, cx: &mut VisualTestContext) {
        self.store
            .update(cx, |store, cx| store.show_alone(thread.to_owned(), cx));
        chat.update(cx, |chat, cx| chat.set_session(Some(thread.to_owned()), cx));
    }

    /// The subscriptions to `thread`'s events the client sent.
    fn events_subscriptions(&self, thread: &str) -> Vec<Subscription> {
        let topic = Topic::SessionEvents {
            session_id: thread.to_owned(),
        };
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| match &message.payload {
                ClientPayload::Subscribe(subscription) if subscription.topic == topic => {
                    Some(subscription.clone())
                }
                _ => None,
            })
            .collect()
    }
}

/// Where every row painted in the last frame was, by row.
fn painted(
    chat: &Entity<ChatView>,
    cx: &mut VisualTestContext,
) -> Vec<(usize, gpui::Bounds<gpui::Pixels>)> {
    let rows = chat
        .read_with(cx, |chat, _| chat.timeline())
        .read_with(cx, |chat, _| chat.rows.len());
    (0..rows)
        .filter_map(|row| {
            cx.debug_bounds(format!("timeline-row-{row}").leak())
                .map(|bounds| (row, bounds))
        })
        .collect()
}

/// A thread that ran on while another was shown, come back to, shows its
/// latest entry, brought there by one subscription. So for a run that fits one
/// window (ten records, which the window continues) and for one longer than
/// a window (140, answered with the thread's tail).
#[gpui::test]
fn a_thread_come_back_to_shows_its_latest_entry_brought_by_one_subscription(
    cx: &mut TestAppContext,
) {
    // The history a tail window leaves above it loads behind a spinner,
    // which rests only with motion reduced.
    cx.update(|cx| cx.set_reduce_motion(true));
    for (count, continued) in [(10, true), (140, false)] {
        let mut workspace = Workspace::new(cx);
        let window_state = cx.new(|_| WindowState::default());
        let store = workspace.store.clone();
        let window =
            cx.add_window(|window, cx| ChatView::new(store, window_state, None, window, cx));
        let chat = window.root(cx).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(size(px(1_024.), px(700.)));
        let (first, second) = (workspace.first.clone(), workspace.second.clone());
        workspace.show(&chat, &first, cx);
        let store = workspace.store.clone();
        workspace.driver.wait_until(cx, "the first thread", |cx| {
            workspace_last(&store, &first, cx).as_deref() == Some("answer-2")
        });
        draw_settled(cx);
        workspace.show(&chat, &second, cx);
        workspace.driver.wait_until(cx, "the second thread", |cx| {
            store.read_with(cx, |store, cx| !store.chat_loading_for(Some(&second), cx))
        });
        draw_settled(cx);

        let _provider = workspace.provider(&first, long_run(3, count), cx);
        workspace.sent.lock().unwrap().clear();
        workspace.show(&chat, &first, cx);
        workspace
            .driver
            .wait_until(cx, "the first thread's window", |cx| {
                workspace_last(&store, &first, cx).as_deref() == Some("finished")
            });
        let asked = workspace.events_subscriptions(&first);
        assert_eq!(asked.len(), 1, "{count}: one window brought it: {asked:?}");
        assert!(
            asked[0].after.is_some(),
            "{count}: it asked from what it kept"
        );
        let held_from = store.read_with(cx, |store, _| store.history_available_for(Some(&first)));
        assert_eq!(
            held_from, !continued,
            "{count}: a run that fits is continued, a longer one is the tail"
        );

        draw_settled(cx);
        let latest = chat
            .read_with(cx, |chat, _| chat.timeline())
            .read_with(cx, |chat, _| chat.rows.len() - 1);
        assert_eq!(
            painted(&chat, cx).last().map(|(row, _)| *row),
            Some(latest),
            "{count}: the window shows the latest entry"
        );

        workspace.driver.shutdown(cx, &workspace.host);
    }
}

fn workspace_last(
    store: &Entity<WorkspaceStore>,
    thread: &str,
    cx: &TestAppContext,
) -> Option<String> {
    store.read_with(cx, |store, cx| {
        store
            .with_timeline_for(
                Some(thread),
                |timeline| timeline.entries.last().map(|entry| entry.id.clone()),
                cx,
            )
            .flatten()
    })
}
